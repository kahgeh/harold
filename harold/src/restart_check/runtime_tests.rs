use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::*;
use crate::pod_roles::{PodRoles, PodStart};
use crate::tools::job::process::ProcessPort;

struct Processes(Mutex<HashMap<u32, i64>>);
impl ProcessPort for Processes {
    fn start_time(&self, pid: u32) -> Option<i64> {
        self.0.lock().unwrap().get(&pid).copied()
    }
}

/// The multiplexer's view: the agent pid of each pane.
#[derive(Default)]
struct View(Mutex<HashMap<String, u32>>);
impl PaneView for View {
    fn agent_pid(&self, pane: &str) -> Result<Option<u32>, ViewError> {
        Ok(self.0.lock().unwrap().get(pane).copied())
    }
    fn session_label(&self, _pane: &str) -> Option<String> {
        Some("harold".into())
    }
}

struct Clock;
impl TimeSource for Clock {
    fn now(&self) -> (String, String) {
        ("2026-11-14 16:05".into(), "20261114-1605".into())
    }
}

#[derive(Default)]
struct Delivered {
    typed: Mutex<Vec<(String, String)>>,
    phoned: Mutex<Vec<(String, String)>>,
    idle: bool,
}
#[tonic::async_trait]
impl Deliver for Delivered {
    async fn type_line(&self, pane: &str, text: &str) -> bool {
        self.typed.lock().unwrap().push((pane.into(), text.into()));
        self.idle
    }
    async fn phone_line(&self, source_pane: &str, text: &str) -> bool {
        self.phoned
            .lock()
            .unwrap()
            .push((source_pane.into(), text.into()));
        true
    }
}

struct World {
    dir: std::path::PathBuf,
    roles: Arc<PodRoles>,
    view: Arc<View>,
    processes: Arc<Processes>,
    delivered: Arc<Delivered>,
    checker: Arc<Checker>,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn world(idle: bool) -> World {
    let dir = std::env::temp_dir().join(format!("harold-restart-run-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    let processes = Arc::new(Processes(Mutex::new(
        [(100, 1), (200, 2), (300, 3), (400, 4)]
            .into_iter()
            .collect(),
    )));
    let roles = Arc::new(PodRoles::open(&dir, processes.clone()));
    let view = Arc::new(View::default());
    let delivered = Arc::new(Delivered {
        idle,
        ..Delivered::default()
    });
    let checker = Arc::new(Checker::new(
        roles.clone(),
        processes.clone(),
        view.clone(),
        dir.join("sessions"),
        Arc::new(Clock),
        delivered.clone(),
    ));
    World {
        dir,
        roles,
        view,
        processes,
        delivered,
        checker,
    }
}

impl World {
    fn start(&self, role: &str, product: &str, spec: &str, pid: u32, pane: &str) {
        self.view.0.lock().unwrap().insert(pane.into(), pid);
        let folder = self.dir.join(format!("w{pid}"));
        std::fs::create_dir_all(&folder).unwrap();
        self.roles
            .record(
                PodStart {
                    role: role.into(),
                    product: product.into(),
                    pane_id: pane.into(),
                    session_id: format!("s-{pid}"),
                    transcript_path: String::new(),
                    folder: folder.to_string_lossy().into_owned(),
                    agent_pid: pid,
                    start_file: String::new(),
                    source: "startup".into(),
                    spec: spec.into(),
                },
                1,
            )
            .unwrap();
    }
    fn pod(&self) {
        self.start("chief", "", "", 100, "%1");
        self.start("pnem", "harold", "", 200, "%2");
        self.start("pod_coordination", "harold", "", 300, "%3");
    }
}

fn pnem() -> Agent {
    Agent::Pnem("harold".into())
}

#[tokio::test]
async fn an_on_demand_check_of_a_whole_pnem_prints_ok_and_writes_the_same_text_as_the_receipt() {
    let world = world(true);
    world.pod();
    let done = world.checker.run(&pnem()).await.unwrap();
    assert!(
        done.text.starts_with(
            "restart check: pnem harold  (session harold, pane 2)  2026-11-14 16:05\n"
        )
    );
    assert!(
        done.text.ends_with("result: ok\n") || done.text.contains("not checked"),
        "{}",
        done.text
    );
    assert_eq!(done.fails, 0);
    let receipt = world
        .dir
        .join("w200/.superpowers/pod/restarts/pnem-20261114-1605.md");
    assert_eq!(std::fs::read_to_string(receipt).unwrap(), done.text);
    assert!(world.delivered.typed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_record_with_an_old_pane_is_corrected_and_the_row_shows_fixed() {
    let world = world(true);
    world.pod();
    world.view.0.lock().unwrap().insert("%9".into(), 200);
    world.view.0.lock().unwrap().remove("%2");
    let done = world.checker.run_at(&pnem(), "%9", 200).await.unwrap();
    assert!(done.text.contains("fixed"), "{}", done.text);
    assert_eq!(world.roles.get("harold", "pnem").unwrap().pane_id, "%9");
    assert_eq!(done.fails, 0);
}

#[tokio::test]
async fn a_pnem_fail_is_typed_into_the_chief_pane_as_one_clean_line() {
    let world = world(true);
    world.pod();
    world.processes.0.lock().unwrap().remove(&100);
    let done = world.checker.run(&pnem()).await.unwrap();
    assert!(done.fails >= 1);
    let typed = world.delivered.typed.lock().unwrap().clone();
    assert_eq!(typed.len(), 1);
    assert_eq!(typed[0].0, "%1");
    assert!(typed[0].1.starts_with("restart check pnem harold: "));
    assert!(!typed[0].1.contains('\n'));
    assert!(
        typed[0]
            .1
            .ends_with(&format!("result: {} FAIL", done.fails))
    );
    assert!(world.delivered.phoned.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_busy_pane_gets_nothing_typed_and_the_check_still_ends() {
    let world = world(false);
    world.pod();
    world.processes.0.lock().unwrap().remove(&300);
    let done = world.checker.run(&pnem()).await.unwrap();
    assert!(done.fails >= 1);
    assert!(std::fs::read_dir(world.dir.join("w200/.superpowers/pod/restarts")).is_ok());
}

#[tokio::test]
async fn the_chiefs_own_fail_goes_to_the_phone_and_is_never_typed() {
    let world = world(true);
    world.pod();
    world.processes.0.lock().unwrap().remove(&300);
    let done = world.checker.run(&Agent::Chief).await.unwrap();
    assert!(done.fails >= 1);
    assert!(world.delivered.typed.lock().unwrap().is_empty());
    let phoned = world.delivered.phoned.lock().unwrap().clone();
    assert_eq!(phoned.len(), 1);
    assert_eq!(phoned[0].0, "%1");
}

#[tokio::test]
async fn a_build_worker_fail_goes_to_pod_coordination() {
    let world = world(true);
    world.pod();
    world.start("build_worker", "harold", "spec-a", 400, "%4");
    world.view.0.lock().unwrap().insert("%4".into(), 999);
    let worker = Agent::BuildWorker {
        product: "harold".into(),
        spec: "spec-a".into(),
    };
    let done = world.checker.run_at(&worker, "%4", 400).await.unwrap();
    assert!(done.fails >= 1);
    let typed = world.delivered.typed.lock().unwrap().clone();
    assert_eq!(typed[0].0, "%3");
    let receipt = world
        .dir
        .join("w300/.superpowers/pod/restarts/worker-spec-a-20261114-1605.md");
    assert!(receipt.is_file());
}

#[tokio::test]
async fn no_record_gives_no_check() {
    let world = world(true);
    assert!(world.checker.run(&pnem()).await.is_none());
}

#[tokio::test]
async fn a_restart_runs_one_check_at_the_first_turn_end_of_the_new_session_only() {
    let world = world(true);
    world.pod();
    world
        .checker
        .note_start(&pnem(), Some("s-old".into()), "s-200", 200, "startup");
    world.checker.on_turn_end("%2", "s-other").await;
    assert!(!world.dir.join("w200/.superpowers/pod/restarts").exists());
    world.checker.on_turn_end("%2", "s-200").await;
    assert!(world.dir.join("w200/.superpowers/pod/restarts").is_dir());
    let count = |d: &std::path::Path| std::fs::read_dir(d).unwrap().count();
    world.checker.on_turn_end("%2", "s-200").await;
    assert_eq!(count(&world.dir.join("w200/.superpowers/pod/restarts")), 1);
}

#[tokio::test]
async fn a_first_start_the_same_session_and_a_clear_or_compact_are_no_restart() {
    let world = world(true);
    world.pod();
    world
        .checker
        .note_start(&pnem(), None, "s-200", 200, "startup");
    world
        .checker
        .note_start(&pnem(), Some("s-200".into()), "s-200", 200, "resume");
    world
        .checker
        .note_start(&pnem(), Some("s-old".into()), "s-200", 200, "clear");
    world
        .checker
        .note_start(&pnem(), Some("s-old".into()), "s-200", 200, "compact");
    world.checker.on_turn_end("%2", "s-200").await;
    assert!(!world.dir.join("w200/.superpowers/pod/restarts").exists());
}

#[tokio::test]
async fn a_pane_that_now_holds_another_agent_is_an_old_agent_and_a_reused_pid_is_gone() {
    let world = world(true);
    world.pod();
    world.view.0.lock().unwrap().insert("%1".into(), 999);
    let done = world.checker.run(&pnem()).await.unwrap();
    assert!(
        done.text.contains("pnem harold -> chief: old_agent: "),
        "{}",
        done.text
    );
    // A pid that runs with another start time than the record holds is gone.
    world.processes.0.lock().unwrap().insert(300, 777);
    let done = world.checker.run(&pnem()).await.unwrap();
    assert!(
        done.text.contains("pod-coordination harold: agent_gone: "),
        "{}",
        done.text
    );
}
