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
async fn a_bucket_that_cannot_be_reached_stops_the_tool_with_its_reason() {
    let (vault, secrets, data) = (TempFolder::new(), TempFolder::new(), TempFolder::new());
    let (_stop, shutdown) = tokio::sync::watch::channel(());
    let started = start(launch(&vault, &secrets, 0o600), data.0.clone(), shutdown).await;
    let reason = started.err().map(|e| e.reason);
    assert!(
        matches!(reason, Some("bucket_unreachable" | "store_failed")),
        "{reason:?}"
    );
}
