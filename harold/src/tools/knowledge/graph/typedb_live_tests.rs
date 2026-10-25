//! Runs against a real TypeDB server that each test starts in a temporary folder, on
//! 127.0.0.1 and a free port. A test skips with a note when no server binary is found. Set
//! `HAROLD_TYPEDB_BIN` to the path of `typedb_server_bin`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::super::super::test_support::TempFolder;
use super::super::backend::{GraphBackend, Snapshot};
use super::super::log::RecordLog;
use super::super::log::tests::MemoryLog;
use super::super::records::{By, Op};
use super::super::server::{ServerConfig, TypeDbServer};
use super::super::writer::{GraphError, GraphWriter};
use super::TypeDbBackend;

fn binary() -> Option<PathBuf> {
    let found = std::env::var_os("HAROLD_TYPEDB_BIN")
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from("/opt/homebrew/bin/typedb_server_bin")))
        .filter(|path| path.is_file());
    if found.is_none() {
        eprintln!("skipped: no TypeDB server binary (set HAROLD_TYPEDB_BIN)");
    }
    found
}

struct Rig {
    server: Arc<TypeDbServer>,
    backend: Arc<TypeDbBackend>,
    schema: Arc<MemoryLog>,
    data: Arc<MemoryLog>,
    _folder: TempFolder,
}

impl Rig {
    fn new(binary: PathBuf) -> Self {
        let folder = TempFolder::new();
        let server = TypeDbServer::new(ServerConfig {
            binary,
            folder: folder.0.join("typedb"),
            idle: Duration::from_secs(600),
        });
        Self {
            backend: Arc::new(TypeDbBackend::new(server.clone())),
            server,
            schema: Arc::default(),
            data: Arc::default(),
            _folder: folder,
        }
    }

    async fn writer(&self) -> Result<GraphWriter, GraphError> {
        let schema: Arc<dyn RecordLog> = self.schema.clone();
        let data: Arc<dyn RecordLog> = self.data.clone();
        GraphWriter::start(self.backend.clone(), schema, data).await
    }
}

fn note(name: &str, title: &str) -> Op {
    Op::PutVaultNote {
        name: name.into(),
        title: title.into(),
        kind: "lesson".into(),
    }
}

fn relation(from: &str, kind: &str, to: &str) -> Op {
    Op::PutRelation {
        from: from.into(),
        relation: kind.into(),
        to: to.into(),
    }
}

async fn fill(writer: &GraphWriter) {
    for (name, title) in [
        ("a", "Plain"),
        ("b", "Quote \" and back\\slash"),
        ("c/d", "Semi; $x match delete"),
    ] {
        writer.submit(note(name, title), By::Index).await.unwrap();
    }
    for op in [
        Op::PutLink {
            from: "a".into(),
            to: "b".into(),
        },
        relation("a", "supersedes", "b"),
        relation("b", "part_of", "c/d"),
    ] {
        assert!(writer.submit(op, By::Call).await.unwrap().is_some());
    }
}

#[tokio::test]
async fn a_real_typedb_holds_notes_links_and_relations_and_puts_twice_without_a_change() {
    let Some(binary) = binary() else { return };
    let rig = Rig::new(binary);
    let writer = rig.writer().await.unwrap();
    fill(&writer).await;
    assert_eq!(
        writer.submit(note("a", "Plain"), By::Index).await.unwrap(),
        None
    );
    assert_eq!(
        writer
            .submit(relation("a", "supersedes", "b"), By::Call)
            .await
            .unwrap(),
        None
    );
    let snapshot = writer.snapshot().await.unwrap();
    assert_eq!(snapshot.notes.len(), 3);
    assert_eq!(snapshot.notes["b"].title, "Quote \" and back\\slash");
    assert_eq!(snapshot.notes["c/d"].title, "Semi; $x match delete");
    assert_eq!(snapshot.links.len(), 1);
    assert_eq!(snapshot.relations.len(), 2);
    let rows = writer.relations_of("b").await.unwrap();
    let seen: Vec<_> = rows
        .iter()
        .map(|r| (r.relation.as_str(), r.other.as_str(), r.outgoing))
        .collect();
    assert_eq!(
        seen,
        [
            ("links", "a", false),
            ("part_of", "c/d", true),
            ("supersedes", "a", false)
        ]
    );
    let status = writer.status().await.unwrap();
    assert_eq!(status.waiting, 0);
    assert_eq!(status.data_position, status.data_end);
    rig.server.stop().await;
}

#[tokio::test]
async fn a_real_typedb_refuses_a_relation_to_a_missing_note_and_keeps_a_named_note() {
    let Some(binary) = binary() else { return };
    let rig = Rig::new(binary);
    let writer = rig.writer().await.unwrap();
    fill(&writer).await;
    let missing = writer
        .submit(relation("a", "relates_to", "nobody"), By::Call)
        .await;
    assert!(
        matches!(missing, Err(GraphError::UnknownVaultNote)),
        "{missing:?}"
    );
    let removed = writer
        .submit(Op::RemoveVaultNote { name: "b".into() }, By::Index)
        .await;
    assert!(
        matches!(removed, Err(GraphError::InvalidVaultNote)),
        "{removed:?}"
    );
    writer
        .submit(Op::MarkVaultNoteMissing { name: "b".into() }, By::Index)
        .await
        .unwrap();
    assert!(writer.snapshot().await.unwrap().notes["b"].missing);
    // Unrelate works for a vault note that is gone from the vault.
    writer
        .submit(
            Op::RemoveRelation {
                from: "b".into(),
                relation: "part_of".into(),
                to: "c/d".into(),
            },
            By::Call,
        )
        .await
        .unwrap();
    writer
        .submit(
            Op::RemoveRelation {
                from: "a".into(),
                relation: "supersedes".into(),
                to: "b".into(),
            },
            By::Call,
        )
        .await
        .unwrap();
    writer
        .submit(Op::RemoveVaultNote { name: "b".into() }, By::Index)
        .await
        .unwrap();
    let snapshot = writer.snapshot().await.unwrap();
    assert!(!snapshot.notes.contains_key("b"));
    assert!(snapshot.links.is_empty(), "the link to b went with it");
    assert_eq!(
        writer
            .submit(Op::RemoveVaultNote { name: "b".into() }, By::Index)
            .await
            .unwrap(),
        None
    );
    rig.server.stop().await;
}

#[tokio::test]
async fn a_real_typedb_comes_back_the_same_after_a_stop_a_rebuild_and_a_lost_database() {
    let Some(binary) = binary() else { return };
    let rig = Rig::new(binary);
    let writer = rig.writer().await.unwrap();
    fill(&writer).await;
    let before: Snapshot = writer.snapshot().await.unwrap();
    // The server stops as it does when idle, and the next call starts it again.
    rig.server.stop().await;
    assert_eq!(writer.snapshot().await.unwrap(), before);
    // A rebuild from the logs gives the same graph.
    let rebuilt = writer.rebuild().await.unwrap();
    assert_eq!(rebuilt.applied.schema_steps, 1);
    assert_eq!(writer.snapshot().await.unwrap(), before);
    // A new writer finds the graph in step and applies nothing.
    drop(writer);
    let again = rig.writer().await.unwrap();
    assert_eq!(again.snapshot().await.unwrap(), before);
    // A lost database is made again from the logs on start.
    drop(again);
    rig.backend.drop_database().await.unwrap();
    let remade = rig.writer().await.unwrap();
    assert_eq!(remade.snapshot().await.unwrap(), before);
    rig.server.stop().await;
}

#[tokio::test]
async fn a_real_typedb_signs_in_with_a_private_password_and_listens_on_loopback_only() {
    let Some(binary) = binary() else { return };
    let rig = Rig::new(binary);
    let lease = rig.server.lease().await.unwrap();
    assert!(lease.base.starts_with("http://127.0.0.1:"));
    let file = rig._folder.0.join("typedb/password");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let default = rig
        .server
        .client()
        .post(format!("{}/v1/signin", lease.base))
        .json(&serde_json::json!({"username": "admin", "password": "password"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        default.status().as_u16(),
        401,
        "the default password is replaced"
    );
    rig.server.stop().await;
    // A second start reuses the saved password.
    assert!(rig.server.lease().await.is_ok());
    rig.server.stop().await;
}
