use std::os::unix::fs::PermissionsExt;

use super::test_support::TempFolder;
use super::*;
use crate::settings::KnowledgeBucketSettings;

fn launch(vault: &TempFolder, secrets: &TempFolder, credentials_mode: u32) -> Launch {
    let credentials = secrets.0.join("credentials");
    std::fs::write(&credentials, "access_key=a\nsecret_key=b\n").unwrap();
    std::fs::set_permissions(
        &credentials,
        std::fs::Permissions::from_mode(credentials_mode),
    )
    .unwrap();
    let key = secrets.0.join("key");
    std::fs::write(&key, "ab".repeat(32)).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    Launch {
        settings: KnowledgeSettings {
            vault: Some(vault.0.to_string_lossy().into_owned()),
            exclude: Vec::new(),
            command: Some("python3".into()),
            args: Vec::new(),
            model: Some("/no/such/model.gguf".into()),
            model_sha256: None,
            bucket: Some(KnowledgeBucketSettings {
                endpoint: "http://127.0.0.1:1".into(),
                bucket: "b".into(),
                prefix: None,
                credentials_file: credentials.to_string_lossy().into_owned(),
                encryption_key_file: key.to_string_lossy().into_owned(),
            }),
            graph: Default::default(),
        },
        machine_name: "m".into(),
    }
}

#[tokio::test]
async fn a_credentials_file_others_can_read_stops_the_tool_from_starting() {
    let (vault, secrets, data) = (TempFolder::new(), TempFolder::new(), TempFolder::new());
    let (_stop, shutdown) = tokio::sync::watch::channel(());
    let started = start(launch(&vault, &secrets, 0o644), data.0.clone(), shutdown).await;
    assert_eq!(started.err().map(|e| e.reason), Some("store_failed"));
    assert!(!data.0.join("index").exists(), "nothing was opened");
}

#[tokio::test]
async fn a_secret_file_inside_the_vault_stops_the_tool_from_starting() {
    let (vault, data) = (TempFolder::new(), TempFolder::new());
    let inside = TempFolder(vault.0.join("secrets"));
    std::fs::create_dir_all(&inside.0).unwrap();
    let (_stop, shutdown) = tokio::sync::watch::channel(());
    let started = start(launch(&vault, &inside, 0o600), data.0.clone(), shutdown).await;
    assert_eq!(started.err().map(|e| e.reason), Some("store_failed"));
}

#[tokio::test]
async fn a_bucket_that_cannot_be_reached_at_start_leaves_the_tool_up_and_answering_unavailable() {
    use harold_api::tools::knowledge::v1::knowledge_service_client::KnowledgeServiceClient;
    let (vault, secrets, data) = (TempFolder::new(), TempFolder::new(), TempFolder::new());
    let (stop, shutdown) = tokio::sync::watch::channel(());
    let handle = start(launch(&vault, &secrets, 0o600), data.0.clone(), shutdown)
        .await
        .map_err(|e| e.reason)
        .expect("the tool starts without its bucket");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (incoming, connections) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            if incoming
                .send(Ok::<_, std::io::Error>(socket))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let routes = handle.routes;
    let server = tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_routes(routes)
            .serve_with_incoming(tokio_stream::wrappers::ReceiverStream::new(connections))
            .await;
    });
    let mut client = KnowledgeServiceClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    // The open is still trying, or has failed once: either way it is not open.
    for _ in 0..2 {
        let refused = client
            .recent(harold_api::tools::knowledge::v1::RecentRequest { limit: 10 })
            .await
            .unwrap_err();
        assert_eq!(refused.code(), tonic::Code::Unavailable);
        assert!(refused.message().starts_with("store_failed"), "{refused:?}");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    server.abort();
    let _ = stop.send(());
    let _ = handle.task.await;
}
