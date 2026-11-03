use std::path::PathBuf;

use super::*;
use crate::tools::telemetry::name::parse_attr;

struct TempFolder(PathBuf);

impl TempFolder {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-telemetry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn declaration(name: &str, kind: Kind) -> Declaration {
    Declaration {
        name: name.into(),
        kind,
        meaning: "Release attempts, by result".into(),
        attrs: vec![
            parse_attr("result", &["ok".to_string(), "failed".to_string()]).unwrap(),
            parse_attr("step", &["1-60".to_string()]).unwrap(),
        ],
        created_ms: 42,
        last_value: None,
    }
}

async fn names(store: &TursoTelemetryStore, prefix: Option<&str>) -> Vec<String> {
    store
        .list(prefix)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.name)
        .collect()
}

#[tokio::test]
async fn put_get_list_remove_round_trip() {
    let folder = TempFolder::new();
    let store = TursoTelemetryStore::open(&folder.0).await.unwrap();
    assert!(folder.0.join("telemetry.db").exists());
    let one = declaration("harold.b", Kind::Counter);
    store.put(&one).await.unwrap();
    store
        .put(&declaration("harold.a", Kind::Event))
        .await
        .unwrap();
    store
        .put(&declaration("chief.c", Kind::Gauge))
        .await
        .unwrap();

    assert_eq!(store.get("harold.b").await.unwrap(), Some(one));
    assert_eq!(store.get("harold.zz").await.unwrap(), None);
    assert_eq!(
        names(&store, None).await,
        ["chief.c", "harold.a", "harold.b"]
    );
    assert_eq!(
        names(&store, Some("harold")).await,
        ["harold.a", "harold.b"]
    );
    assert!(names(&store, Some("web")).await.is_empty());

    assert!(store.remove("harold.a").await.unwrap());
    assert!(!store.remove("harold.a").await.unwrap());
    assert_eq!(names(&store, Some("harold")).await, ["harold.b"]);
}

#[tokio::test]
async fn a_reopened_file_gives_the_data_back() {
    let folder = TempFolder::new();
    let store = TursoTelemetryStore::open(&folder.0).await.unwrap();
    let one = declaration("harold.b", Kind::Gauge);
    store.put(&one).await.unwrap();
    assert!(store.set_last_value("harold.b", 2.5).await.unwrap());
    drop(store);

    let store = TursoTelemetryStore::open(&folder.0).await.unwrap();
    let got = store.get("harold.b").await.unwrap().unwrap();
    assert_eq!(got.attrs, one.attrs);
    assert_eq!(got.created_ms, 42);
    assert_eq!(got.last_value, Some(2.5));
}

#[tokio::test]
async fn counts_are_for_one_prefix() {
    let folder = TempFolder::new();
    let store = TursoTelemetryStore::open(&folder.0).await.unwrap();
    assert_eq!(store.count_for_prefix("harold").await.unwrap(), (0, 0));
    store
        .put(&declaration("harold.a", Kind::Event))
        .await
        .unwrap();
    store
        .put(&declaration("harold.b", Kind::Event))
        .await
        .unwrap();
    store
        .put(&declaration("harold.c", Kind::Counter))
        .await
        .unwrap();
    store
        .put(&declaration("chief.a", Kind::Event))
        .await
        .unwrap();
    assert_eq!(store.count_for_prefix("harold").await.unwrap(), (3, 2));
    assert_eq!(store.count_for_prefix("chief").await.unwrap(), (1, 1));
}

#[tokio::test]
async fn the_last_value_is_set_and_a_put_clears_it() {
    let folder = TempFolder::new();
    let store = TursoTelemetryStore::open(&folder.0).await.unwrap();
    assert!(!store.set_last_value("harold.g", 1.0).await.unwrap());
    store
        .put(&declaration("harold.g", Kind::Gauge))
        .await
        .unwrap();
    assert_eq!(
        store.get("harold.g").await.unwrap().unwrap().last_value,
        None
    );
    store.set_last_value("harold.g", -3.0).await.unwrap();
    store.set_last_value("harold.g", 7.0).await.unwrap();
    assert_eq!(
        store.get("harold.g").await.unwrap().unwrap().last_value,
        Some(7.0)
    );
    store
        .put(&declaration("harold.g", Kind::Gauge))
        .await
        .unwrap();
    assert_eq!(
        store.get("harold.g").await.unwrap().unwrap().last_value,
        None
    );
}

#[tokio::test]
async fn open_refuses_a_schema_it_does_not_know() {
    let folder = TempFolder::new();
    let store = TursoTelemetryStore::open(&folder.0).await.unwrap();
    {
        let conn = store.conn.lock().await;
        conn.execute(
            "INSERT INTO _migrations (name, checksum, applied_at_ms) VALUES ('999_x', 'x', 1)",
            (),
        )
        .await
        .unwrap();
    }
    drop(store);
    assert!(TursoTelemetryStore::open(&folder.0).await.is_err());
}
