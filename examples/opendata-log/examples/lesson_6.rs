use bytes::Bytes;
use opendata_agent_progress::{Checkpoints, Result, config, reader_config};
use opendata_common::StorageConfig;
use opendata_log::{LogDb, LogDbReader, LogRead, Record};
use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::mpsc,
    time::{sleep, timeout},
};

#[tokio::main]
async fn main() -> Result<()> {
    timeout(Duration::from_secs(90), run()).await?
}

async fn run() -> Result<()> {
    let config = config("lesson-6")?;
    if matches!(config.storage, StorageConfig::InMemory) {
        return Err("Use local, s3, or express: independently opened in-memory readers do not share storage".into());
    }
    let log = LogDb::open(config.clone()).await?;
    log.flush().await?;
    let reader = LogDbReader::open(reader_config(&config)).await?;
    // New keys isolate this run from previous runs without deleting stored history.
    let run = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let keys: Vec<_> = ["agent-a", "agent-b"]
        .iter()
        .map(|agent| format!("{run}/{agent}"))
        .collect();
    let (sender, mut queue) = mpsc::channel::<Record>(2);
    let mut producers = Vec::new();
    for key in keys.clone() {
        let sender = sender.clone();
        producers.push(tokio::spawn(async move {
            for message in ["Started", "Progress", "Finished"] {
                // Awaiting a full queue slows the producer instead of dropping a report.
                sender
                    .send(Record {
                        key: Bytes::from(key.clone()),
                        value: Bytes::from_static(message.as_bytes()),
                    })
                    .await?;
                sleep(Duration::from_millis(30)).await;
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        }));
    }
    drop(sender);
    let writer = tokio::spawn(async move {
        while let Some(record) = queue.recv().await {
            let result = log.try_append(vec![record]).await?;
            log.flush().await?;
            println!("Writer durably acknowledged [{}]", result.start_sequence);
        }
        log.close().await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    });
    let monitored = timeout(Duration::from_secs(30), async {
        let mut checkpoints = Checkpoints::default();
        let mut observed: BTreeMap<String, Vec<String>> = BTreeMap::new();
        loop {
            for key in &keys {
                let mut records = reader
                    .scan(Bytes::from(key.clone()), checkpoints.position(key)..)
                    .await?;
                while let Some(record) = records.next().await? {
                    let message = std::str::from_utf8(&record.value)?.to_owned();
                    println!("Monitor [{}] {key}: {message}", record.sequence);
                    observed.entry(key.clone()).or_default().push(message);
                    checkpoints.next.insert(key.clone(), record.sequence + 1);
                }
            }
            if observed.values().map(Vec::len).sum::<usize>() >= 6 {
                for key in &keys {
                    assert_eq!(
                        observed.get(key).unwrap(),
                        &["Started", "Progress", "Finished"]
                    );
                }
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await;
    reader.close().await;
    // On monitor failure, cancel owned tasks instead of leaving background writers running.
    if !matches!(&monitored, Ok(Ok(()))) {
        writer.abort();
        for producer in &producers {
            producer.abort();
        }
    }
    monitored??;
    for producer in producers {
        producer.await??;
    }
    writer.await??;
    println!("All six reports arrived once, in each agent's order.");
    Ok(())
}
