use super::*;

struct Temp(std::path::PathBuf);

impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-jobdb-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn job(name: &str) -> NewJob {
    NewJob {
        name: name.into(),
        what: "".into(),
        pane_id: "%4".into(),
        pid: 4121,
        process_start_s: 1_700_000_000,
        started_at_ms: 1_000,
        due_at_ms: 1_000 + 20 * 60_000,
    }
}

#[tokio::test]
async fn add_stores_a_running_job_and_refuses_a_second_of_the_same_name() {
    let dir = Temp::new();
    let store = JobStore::open(&dir.0).await.unwrap();
    assert!(store.add(&job("calibration")).await.unwrap());
    assert!(!store.add(&job("calibration")).await.unwrap());
    let running = store.running().await.unwrap();
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].name, "calibration");
    assert_eq!(running[0].state, JobState::Running);
    assert_eq!(running[0].ended_at_ms, 0);
    assert_eq!(running[0].end_line, None);
}

#[tokio::test]
async fn an_ended_job_waits_for_its_line_until_the_outcome_is_recorded() {
    let dir = Temp::new();
    let store = JobStore::open(&dir.0).await.unwrap();
    store.add(&job("a")).await.unwrap();
    let id = store.running().await.unwrap()[0].id;
    assert!(store.mark_ended(id, 5_000).await.unwrap());
    assert!(!store.mark_ended(id, 6_000).await.unwrap());
    assert_eq!(store.waiting_for_line().await.unwrap().len(), 1);
    store.set_end_line(id, "typed").await.unwrap();
    assert!(store.waiting_for_line().await.unwrap().is_empty());
}

#[tokio::test]
async fn remove_records_removed_and_leaves_no_line_to_type() {
    let dir = Temp::new();
    let store = JobStore::open(&dir.0).await.unwrap();
    store.add(&job("wrong")).await.unwrap();
    let removed = store.remove("wrong", 3_000).await.unwrap().unwrap();
    assert_eq!(removed.state, JobState::Removed);
    assert!(store.remove("wrong", 3_000).await.unwrap().is_none());
    assert!(store.waiting_for_line().await.unwrap().is_empty());
    assert!(store.shown(3_001).await.unwrap().is_empty());
    // The name is free again.
    assert!(store.add(&job("wrong")).await.unwrap());
}

#[tokio::test]
async fn shown_keeps_an_ended_job_for_10_minutes() {
    let dir = Temp::new();
    let store = JobStore::open(&dir.0).await.unwrap();
    store.add(&job("a")).await.unwrap();
    let id = store.running().await.unwrap()[0].id;
    store.mark_ended(id, 100_000).await.unwrap();
    assert_eq!(store.shown(100_000 + SHOW_AFTER_END_MS - 1).await.unwrap().len(), 1);
    assert!(store.shown(100_000 + SHOW_AFTER_END_MS).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_restart_reads_the_jobs_again() {
    let dir = Temp::new();
    {
        let store = JobStore::open(&dir.0).await.unwrap();
        store.add(&job("a")).await.unwrap();
    }
    let store = JobStore::open(&dir.0).await.unwrap();
    let running = store.running().await.unwrap();
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].process_start_s, 1_700_000_000);
}

#[tokio::test]
async fn prune_deletes_only_finished_jobs_older_than_7_days() {
    let dir = Temp::new();
    let store = JobStore::open(&dir.0).await.unwrap();
    store.add(&job("old")).await.unwrap();
    store.add(&job("running")).await.unwrap();
    let id = store.running().await.unwrap()[0].id;
    store.mark_ended(id, 10).await.unwrap();
    store.set_end_line(id, "typed").await.unwrap();
    store.prune(10 + KEEP_MS + 1).await.unwrap();
    let names: Vec<String> = store.running().await.unwrap().into_iter().map(|j| j.name).collect();
    assert_eq!(names, vec!["running".to_string()]);
    assert!(store.fetch("name = 'old'", ()).await.unwrap().is_empty());
}
