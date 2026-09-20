use bytes::Bytes;
use opendata_common::StorageConfig;
use opendata_log::{Config, LogDb, LogRead, Record};
use std::error::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Every run starts empty. Nothing is written to disk in this lesson.
    let log = LogDb::open(Config {
        storage: StorageConfig::InMemory,
        ..Config::default()
    })
    .await?;
    let agent = Bytes::from_static(b"agent-a");

    // One key identifies this agent's stream; each value is a progress report.
    log.try_append(vec![
        Record {
            key: agent.clone(),
            value: Bytes::from_static(b"Started"),
        },
        Record {
            key: agent.clone(),
            value: Bytes::from_static(b"Progress: inspected 2 files"),
        },
    ])
    .await?;
    // Wait for the write pipeline. In-memory storage still cannot survive exit.
    log.flush().await?;

    println!("First monitor read:");
    let next_sequence = read_progress(&log, agent.clone(), 0).await?;
    println!("Monitor checkpoint: next sequence = {next_sequence}\n");

    log.try_append(vec![Record {
        key: agent.clone(),
        value: Bytes::from_static(b"Finished"),
    }])
    .await?;
    log.flush().await?;

    println!("Second monitor read (after another report):");
    let next_sequence = read_progress(&log, agent.clone(), next_sequence).await?;
    println!("Monitor checkpoint: next sequence = {next_sequence}\n");

    println!("Third monitor read (nothing new):");
    read_progress(&log, agent, next_sequence).await?;
    log.close().await?;
    Ok(())
}

async fn read_progress(
    log: &LogDb,
    agent: Bytes,
    start_sequence: u64,
) -> Result<u64, Box<dyn Error>> {
    // The range includes its start. Remember one past the last record we read.
    let mut records = log.scan(agent, start_sequence..).await?;
    let mut next_sequence = start_sequence;
    while let Some(record) = records.next().await? {
        let agent = std::str::from_utf8(&record.key)?;
        let progress = std::str::from_utf8(&record.value)?;
        println!("  [{}] {agent}: {progress}", record.sequence);
        next_sequence = record.sequence + 1;
    }
    if next_sequence == start_sequence {
        println!("  No new reports.");
    }
    Ok(next_sequence)
}
