use std::sync::atomic::{AtomicUsize, Ordering};

use super::super::super::test_support::TempFolder;
use super::super::fake::FakeGraph;
use super::*;

struct Counting(Arc<AtomicUsize>);

#[tonic::async_trait]
impl BackendOpener for Counting {
    async fn open(&self, _folder: &Path) -> Result<Arc<dyn GraphBackend>, GraphError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(FakeGraph::default()))
    }
}

#[tokio::test]
async fn a_graph_that_is_off_answers_graph_off_and_starts_nothing() {
    let runtime = GraphRuntime::off();
    assert!(!runtime.enabled());
    assert!(matches!(runtime.writer().await, Err(GraphError::Off)));
    assert!(runtime.started().is_none());
}

#[tokio::test]
async fn a_graph_that_is_on_starts_on_the_first_call_and_only_once() {
    let folder = TempFolder::new();
    let opened = Arc::new(AtomicUsize::new(0));
    let runtime = GraphRuntime::new(
        GraphFolders::under(&folder.0),
        Arc::new(Counting(opened.clone())),
    );
    assert!(runtime.enabled());
    assert_eq!(
        opened.load(Ordering::SeqCst),
        0,
        "nothing starts before a call"
    );
    assert!(!folder.0.join("graph").exists());
    runtime.writer().await.unwrap();
    runtime.writer().await.unwrap();
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    assert!(runtime.started().is_some());
    assert!(folder.0.join("graph/schema-log").is_dir());
    assert!(folder.0.join("graph/data-log").is_dir());
}
