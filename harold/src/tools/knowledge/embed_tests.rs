use super::*;
use crate::tools::knowledge::test_support::TempFolder;

fn command(python: &str, model: &str, folder: &TempFolder) -> WorkerCommand {
    WorkerCommand {
        python: python.into(),
        args: Vec::new(),
        model_path: PathBuf::from(model),
        model_sha256: None,
        folder: folder.0.join("worker"),
    }
}

#[tokio::test]
async fn a_worker_whose_model_is_missing_is_unavailable_and_the_next_request_tries_again() {
    let folder = TempFolder::new();
    let worker = WorkerEmbedder::new(
        command("python3", "/no/such/model.gguf", &folder),
        IDLE_LIMIT,
    );
    assert_eq!(worker.state(), "idle");
    let error = worker.embed(vec!["hello".into()]).await.unwrap_err();
    assert_eq!(error, EmbedError::Unavailable("failed_to_load"));
    assert_eq!(worker.state(), "unavailable");
    assert_eq!(
        worker.ready().await.unwrap_err(),
        EmbedError::Unavailable("failed_to_load")
    );
}

#[tokio::test]
async fn a_python_that_does_not_exist_is_unavailable() {
    let folder = TempFolder::new();
    let worker = WorkerEmbedder::new(
        command("/no/such/python", "/no/such/model.gguf", &folder),
        IDLE_LIMIT,
    );
    assert_eq!(
        worker.ready().await.unwrap_err(),
        EmbedError::Unavailable("failed_to_load")
    );
}

#[tokio::test]
async fn the_script_is_written_private_to_the_tools_folder() {
    use std::os::unix::fs::PermissionsExt;
    let folder = TempFolder::new();
    let worker = WorkerEmbedder::new(command("python3", "/no/such/model", &folder), IDLE_LIMIT);
    let _ = worker.ready().await;
    let script = folder.0.join("worker/embed_worker.py");
    assert_eq!(
        std::fs::metadata(&script).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(std::fs::read_to_string(script).unwrap(), EMBED_WORKER_PY);
}
