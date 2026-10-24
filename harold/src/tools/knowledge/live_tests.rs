//! A run against the real embedding worker, model and bucket. Ignored: it needs
//! `KI_PYTHON`, `KI_MODEL`, `KI_MODEL_SHA256`, `KI_VAULT` (a scratch copy of the vault),
//! `KI_ENDPOINT`, `KI_BUCKET`, `KI_CREDENTIALS` and `KI_KEY` (secret files, mode 0600,
//! outside any checkout).

use std::time::{Duration, Instant};

use crate::cli::{KnowledgeCommand, ToolCommand, tool};
use crate::settings::{KnowledgeBucketSettings, KnowledgeSettings};

use super::test_support::TempFolder;
use super::{Launch, start};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

fn launch(prefix: &str) -> Launch {
    Launch {
        settings: KnowledgeSettings {
            vault: Some(env("KI_VAULT")),
            exclude: vec!["MEMORY.md".into(), "log.md".into()],
            command: Some(env("KI_PYTHON")),
            args: Vec::new(),
            model: Some(env("KI_MODEL")),
            model_sha256: Some(env("KI_MODEL_SHA256")),
            bucket: Some(KnowledgeBucketSettings {
                endpoint: env("KI_ENDPOINT"),
                bucket: env("KI_BUCKET"),
                prefix: Some(prefix.into()),
                credentials_file: env("KI_CREDENTIALS"),
                encryption_key_file: env("KI_KEY"),
            }),
        },
        machine_name: "test-mac".into(),
    }
}

struct Served {
    address: String,
    stop: tokio::sync::watch::Sender<()>,
    task: tokio::task::JoinHandle<()>,
    server: tokio::task::AbortHandle,
}

async fn serve(prefix: &str, data: &TempFolder) -> Served {
    let (stop, shutdown) = tokio::sync::watch::channel(());
    let handle = start(launch(prefix), data.0.clone(), shutdown)
        .await
        .map_err(|e| e.reason)
        .expect("the knowledge tool started");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
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
    let server = tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_routes(handle.routes)
            .serve_with_incoming(tokio_stream::wrappers::ReceiverStream::new(connections))
            .await;
    });
    Served {
        address,
        stop,
        task: handle.task,
        server: server.abort_handle(),
    }
}

impl Served {
    async fn run(&self, command: KnowledgeCommand) -> Result<String, String> {
        tool(&self.address, &ToolCommand::Knowledge(command), &b""[..]).await
    }

    async fn stop(self) {
        self.server.abort();
        let _ = self.stop.send(());
        let _ = self.task.await;
    }
}

async fn wait_for_notes(served: &Served, at_least: usize) -> String {
    let started = Instant::now();
    loop {
        let status = served
            .run(KnowledgeCommand::Status)
            .await
            .unwrap_or_default();
        let notes = status
            .lines()
            .find_map(|l| l.strip_prefix("vault notes: "))
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or(0);
        let ready = status.contains("last good pass:") && !status.contains("no pass has succeeded");
        if notes >= at_least && ready {
            return status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(300),
            "timed out: {status}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the real model, worker and bucket (KI_* variables)"]
async fn the_real_index_meets_the_acceptance_criteria() {
    let prefix = format!("harold-knowledge/live-{}", uuid::Uuid::new_v4().simple());
    let expected: usize = env("KI_EXPECTED_NOTES").parse().unwrap();
    let data = TempFolder::new();

    // 1, and a build from the vault alone.
    let started = Instant::now();
    let served = serve(&prefix, &data).await;
    let status = wait_for_notes(&served, expected).await;
    println!(
        "first build took {} ms\n{status}",
        started.elapsed().as_millis()
    );
    assert!(
        status.contains(&format!("vault notes: {expected}")),
        "{status}"
    );

    // 2.
    let found = served
        .run(KnowledgeCommand::Search {
            query: "keeping the index safe if the Mac is lost".into(),
            limit: 10,
        })
        .await
        .unwrap();
    println!("search:\n{found}");
    let top: Vec<&str> = found.lines().take(3).collect();
    assert!(
        top.iter()
            .any(|l| l.contains("a-knowledge-index-on-buckwal")),
        "{found}"
    );

    // 4.
    let note = served
        .run(KnowledgeCommand::Show {
            name: "a-knowledge-index-on-buckwal".into(),
        })
        .await
        .unwrap();
    let links_line = note
        .lines()
        .find(|l| l.starts_with("links: "))
        .unwrap()
        .to_string();
    let linked: Vec<&str> = links_line["links: ".len()..].split(", ").collect();
    let links = served
        .run(KnowledgeCommand::Links {
            name: "a-knowledge-index-on-buckwal".into(),
        })
        .await
        .unwrap();
    println!("note {links_line}\nsuggested links:\n{links}");
    for line in links.lines().filter(|l| *l != "no vault notes") {
        let name = line.split_whitespace().nth(1).unwrap();
        assert!(!linked.contains(&name), "{name} is already linked");
    }
    let related = served
        .run(KnowledgeCommand::Related {
            name: "a-knowledge-index-on-buckwal".into(),
            limit: 10,
        })
        .await
        .unwrap();
    println!("related:\n{related}");
    assert!(related.contains("(linked)"));

    // 6: what the dash asks, as the dash asks it: the 10 most recent, a search from the
    // dash, and the absolute file of a vault note.
    {
        use harold_api::tools::knowledge::v1 as pb;
        use harold_api::tools::knowledge::v1::knowledge_service_client::KnowledgeServiceClient;
        let mut client = KnowledgeServiceClient::connect(format!("http://{}", served.address))
            .await
            .unwrap();
        let recent = client
            .recent(pb::RecentRequest { limit: 10 })
            .await
            .unwrap()
            .into_inner()
            .hits;
        assert_eq!(recent.len(), 10);
        assert!(
            recent
                .windows(2)
                .all(|w| w[0].changed_at_ms >= w[1].changed_at_ms)
        );
        let found = client
            .search(pb::SearchRequest {
                query: "keeping the index safe if the Mac is lost".into(),
                limit: 10,
                source: "dash".into(),
            })
            .await
            .unwrap()
            .into_inner()
            .hits;
        assert!(found.len() <= 10 && !found.is_empty());
        let shown = client
            .show_vault_note(pb::ShowVaultNoteRequest {
                name: "references/harold-locations".into(),
            })
            .await
            .unwrap()
            .into_inner();
        println!("dash opens {}", shown.file);
        assert_eq!(
            shown.file,
            std::path::Path::new(&env("KI_VAULT"))
                .join("references/harold-locations.md")
                .to_string_lossy()
        );
        assert!(std::path::Path::new(&shown.file).is_file());
    }

    // 3.
    let saved = Instant::now();
    let path = std::path::Path::new(&env("KI_VAULT")).join("zz-live-check.md");
    std::fs::write(
        &path,
        "---\nname: zz-live-check\ndescription: a live check\nkind: idea\n---\n\nThe quokka juggles lanterns beside the harbour.\n",
    )
    .unwrap();
    let found_new = loop {
        let found = served
            .run(KnowledgeCommand::Search {
                query: "quokka juggling lanterns".into(),
                limit: 3,
            })
            .await
            .unwrap();
        if found
            .lines()
            .next()
            .is_some_and(|l| l.contains("zz-live-check"))
        {
            break saved.elapsed();
        }
        assert!(saved.elapsed() < Duration::from_secs(30), "{found}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    println!(
        "a saved vault note was found after {} ms",
        found_new.as_millis()
    );
    assert!(found_new < Duration::from_secs(5));
    std::fs::remove_file(&path).unwrap();
    let gone = Instant::now();
    loop {
        let status = served.run(KnowledgeCommand::Status).await.unwrap();
        if status.contains(&format!("vault notes: {expected}")) {
            break;
        }
        assert!(gone.elapsed() < Duration::from_secs(30), "{status}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    served.stop().await;

    // The database kept in the bucket comes back when the local copy is deleted.
    let restored = TempFolder::new();
    let served = serve(&prefix, &restored).await;
    let status = wait_for_notes(&served, expected).await;
    println!("restored from the bucket:\n{status}");
    served.stop().await;

    // 7. Everything deleted: a fresh prefix and folder rebuild from the vault.
    let fresh = TempFolder::new();
    let served = serve(&format!("{prefix}-again"), &fresh).await;
    let status = wait_for_notes(&served, expected).await;
    println!("rebuilt from the vault:\n{status}");
    let rebuilt = served.run(KnowledgeCommand::Rebuild).await.unwrap();
    println!("{rebuilt}");
    served.stop().await;
}
