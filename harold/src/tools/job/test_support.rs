//! Fake ports for the job tool's tests. No test types into a real pane or reads a real
//! process.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use super::process::ProcessPort;
use super::store::JobStore;
use crate::tools::{BoxFuture, PaneTyping, Typing};

/// A process table that tests change by hand.
#[derive(Default)]
pub(super) struct FakeProcesses(pub(super) Mutex<HashMap<u32, i64>>);

impl ProcessPort for FakeProcesses {
    fn start_time(&self, pid: u32) -> Option<i64> {
        self.0.lock().unwrap().get(&pid).copied()
    }
}

/// A pane port that answers from a queue and records each line it was asked to type.
/// With an empty queue it types.
#[derive(Default)]
pub(super) struct FakePane {
    pub(super) answers: Mutex<VecDeque<Typing>>,
    pub(super) asked: Mutex<Vec<(String, String)>>,
}

impl FakePane {
    pub(super) fn lines(&self) -> Vec<(String, String)> {
        self.asked.lock().unwrap().clone()
    }
}

impl PaneTyping for FakePane {
    fn type_line<'a>(&'a self, pane_id: &'a str, text: &'a str) -> BoxFuture<'a, Typing> {
        self.asked
            .lock()
            .unwrap()
            .push((pane_id.to_string(), text.to_string()));
        let answer = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Typing::Typed {
                insert_pressed: false,
            });
        Box::pin(async move { answer })
    }
}

pub(super) struct Temp(pub(super) std::path::PathBuf);

impl Temp {
    pub(super) fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-jobs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(super) async fn store(&self) -> Arc<JobStore> {
        Arc::new(JobStore::open(&self.0).await.unwrap())
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
