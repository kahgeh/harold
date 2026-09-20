use bytes::Bytes;
use opendata_common::StorageConfig;
use opendata_log::{Config, LogDb, LogRead, Record};
use std::error::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let log = LogDb::open(Config {
        storage: StorageConfig::InMemory,
        ..Config::default()
    })
    .await?;

    // The agents take turns so the example produces the same order every run.
    let reports = [
        ("agent-a", "Started"),
        ("agent-b", "Started"),
        ("agent-a", "Progress: inspected 2 files"),
        ("agent-b", "Progress: ran 3 checks"),
        ("agent-a", "Finished"),
        ("agent-b", "Finished"),
    ];

    println!("Append order (one shared log):");
    for (agent, progress) in reports {
        let appended = log
            .try_append(vec![Record {
                key: Bytes::from(agent),
                value: Bytes::from(progress),
            }])
            .await?;
        // Each append has one record, so start_sequence is that record's position.
        println!("  [{}] {agent}: {progress}", appended.start_sequence);
    }
    // Wait for the pipeline before reading. In-memory data still disappears on exit.
    log.flush().await?;

    for agent in ["agent-a", "agent-b"] {
        println!("\nStream {agent}:");
        // The key selects a stream; the open range reads all its records.
        let mut records = log.scan(Bytes::from(agent), ..).await?;
        while let Some(record) = records.next().await? {
            let key = std::str::from_utf8(&record.key)?;
            let progress = std::str::from_utf8(&record.value)?;
            println!("  [{}] {key}: {progress}", record.sequence);
        }
    }

    log.close().await?;
    Ok(())
}
