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
        last_values: LastValues::new(),
    }
}

/// The number of names and, of those, of events for the prefix.
async fn counts(store: &TursoTelemetryStore, prefix: &str) -> (u32, u32) {
    count_in(&*store.conn.lock().await, prefix).await.unwrap()
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
async fn declare_get_list_remove_round_trip() {
    let folder = TempFolder::new();
    let store = TursoTelemetryStore::open(&folder.0).await.unwrap();
    assert!(folder.0.join("telemetry.db").exists());
    let one = declaration("harold.b", Kind::Counter);
    store.declare(&one).await.unwrap();
    store
        .declare(&declaration("harold.a", Kind::Event))
        .await
        .unwrap();
    store
        .declare(&declaration("chief.c", Kind::Gauge))
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
    store.declare(&one).await.unwrap();
    assert!(
        store
            .set_last_value("harold.b", "result=ok", 2.5)
            .await
            .unwrap()
    );
    assert!(
        store
            .set_last_value("harold.b", "result=failed", 1.0)
            .await
            .unwrap()
    );
    drop(store);

    let store = TursoTelemetryStore::open(&folder.0).await.unwrap();
    let got = store.get("harold.b").await.unwrap().unwrap();
    assert_eq!(got.attrs, one.attrs);
    assert_eq!(got.created_ms, 42);
    assert_eq!(
        got.last_values,
        LastValues::from([
            ("result=failed".to_string(), 1.0),
            ("result=ok".to_string(), 2.5)
        ])
    );
}

#[tokio::test]
async fn counts_are_for_one_prefix() {
    let folder = TempFolder::new();
    let store = TursoTelemetryStore::open(&folder.0).await.unwrap();
    assert_eq!(counts(&store, "harold").await, (0, 0));
    store
        .declare(&declaration("harold.a", Kind::Event))
        .await
        .unwrap();
    store
        .declare(&declaration("harold.b", Kind::Event))
        .await
        .unwrap();
    store
        .declare(&declaration("harold.c", Kind::Counter))
        .await
        .unwrap();
    store
        .declare(&declaration("chief.a", Kind::Event))
        .await
        .unwrap();
    assert_eq!(counts(&store, "harold").await, (3, 2));
    assert_eq!(counts(&store, "chief").await, (1, 1));
}

#[tokio::test]
async fn the_last_value_is_set_and_a_new_declaration_clears_it() {
    let folder = TempFolder::new();
    let store = TursoTelemetryStore::open(&folder.0).await.unwrap();
    assert!(!store.set_last_value("harold.g", "", 1.0).await.unwrap());
    store
        .declare(&declaration("harold.g", Kind::Gauge))
        .await
        .unwrap();
    assert_eq!(
        store.get("harold.g").await.unwrap().unwrap().last_values,
        LastValues::new()
    );
    store.set_last_value("harold.g", "", -3.0).await.unwrap();
    store.set_last_value("harold.g", "", 7.0).await.unwrap();
    assert_eq!(
        store.get("harold.g").await.unwrap().unwrap().last_values,
        LastValues::from([(String::new(), 7.0)])
    );
    // The same declaration again changes nothing.
    store
        .declare(&declaration("harold.g", Kind::Gauge))
        .await
        .unwrap();
    assert_eq!(
        store.get("harold.g").await.unwrap().unwrap().last_values,
        LastValues::from([(String::new(), 7.0)])
    );
    // A remove and a new declaration clear it.
    assert!(store.remove("harold.g").await.unwrap());
    store
        .declare(&declaration("harold.g", Kind::Gauge))
        .await
        .unwrap();
    assert_eq!(
        store.get("harold.g").await.unwrap().unwrap().last_values,
        LastValues::new()
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

#[tokio::test]
async fn declare_adds_compares_and_checks_the_caps_in_one_step() {
    let folder = TempFolder::new();
    let store = TursoTelemetryStore::open(&folder.0).await.unwrap();
    let one = declaration("harold.a", Kind::Counter);
    assert_eq!(store.declare(&one).await.unwrap(), Declared::Added);
    let mut again = one.clone();
    again.meaning = "Another meaning".into();
    assert_eq!(store.declare(&again).await.unwrap(), Declared::Same);
    assert_eq!(store.get("harold.a").await.unwrap(), Some(one));
    let other = declaration("harold.a", Kind::Gauge);
    assert_eq!(store.declare(&other).await.unwrap(), Declared::Differs);
    for i in 0..20 {
        let event = declaration(&format!("harold.e{i}"), Kind::Event);
        assert_eq!(store.declare(&event).await.unwrap(), Declared::Added);
    }
    let event = declaration("harold.e20", Kind::Event);
    assert_eq!(
        store.declare(&event).await.unwrap(),
        Declared::TooManyEvents
    );
    for i in 0..29 {
        let counter = declaration(&format!("harold.c{i}"), Kind::Counter);
        assert_eq!(store.declare(&counter).await.unwrap(), Declared::Added);
    }
    let counter = declaration("harold.c29", Kind::Counter);
    assert_eq!(
        store.declare(&counter).await.unwrap(),
        Declared::TooManyNames
    );
    assert_eq!(counts(&store, "harold").await, (50, 20));
}

#[tokio::test]
async fn a_series_key_is_sorted_pairs_joined_by_a_comma() {
    use crate::tools::telemetry::schema::series_key;
    let pairs = |list: &[(&str, &str)]| -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    assert_eq!(series_key(&[]), "");
    assert_eq!(series_key(&pairs(&[("result", "ok")])), "result=ok");
    assert_eq!(
        series_key(&pairs(&[("step", "2"), ("result", "ok")])),
        "result=ok,step=2"
    );
}
