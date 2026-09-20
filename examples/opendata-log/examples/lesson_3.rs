use bytes::Bytes;
use opendata_agent_progress::{Checkpoints, Result};
use opendata_common::StorageConfig;
use opendata_log::{Config, LogDb, LogRead, Record};

#[tokio::main]
async fn main() -> Result<()> {
    let log = LogDb::open(Config {
        storage: StorageConfig::InMemory,
        ..Config::default()
    })
    .await?;
    for (agent, message) in [
        ("agent-a", "Started"),
        ("agent-b", "Started"),
        ("agent-a", "Finished"),
        ("agent-b", "Finished"),
    ] {
        log.try_append(vec![Record {
            key: Bytes::from_static(agent.as_bytes()),
            value: Bytes::from_static(message.as_bytes()),
        }])
        .await?;
    }
    log.flush().await?;
    let mut checkpoints = Checkpoints::default();
    // A is consumed first; B must retain its own lower bound, initially zero.
    for agent in ["agent-a", "agent-b", "agent-a", "agent-b"] {
        println!("Read {agent} from {}", checkpoints.position(agent));
        let mut records = log
            .scan(
                Bytes::from_static(agent.as_bytes()),
                checkpoints.position(agent)..,
            )
            .await?;
        while let Some(record) = records.next().await? {
            println!(
                "  [{}] {}",
                record.sequence,
                std::str::from_utf8(&record.value)?
            );
            checkpoints.next.insert(agent.into(), record.sequence + 1);
        }
    }
    assert_eq!(checkpoints.position("agent-a"), 3);
    assert_eq!(checkpoints.position("agent-b"), 4);
    println!("Separate next positions: {:?}", checkpoints.next);
    log.close().await?;
    Ok(())
}
