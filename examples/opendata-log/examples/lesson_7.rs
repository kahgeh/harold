//! Reconnect to durable execution history, keeping source identity separate from sequence.
use bytes::Bytes;
use opendata_agent_progress::{Result, config, reader_config};
use opendata_log::{LogDb, LogDbReader, LogRead, Record};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct TraceEvent {
    run_id: String,
    event_id: String,
    agent_id: String,
    actor: String,
    frame_id: String,
    caused_by: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    payload: String,
}

impl TraceEvent {
    fn new(run: &str, agent: &str, step: usize) -> Self {
        let (kind, payload) = [
            ("started", "inspect repository"),
            ("tool-request", "list source files"),
            ("tool-result", "found 2 source files"),
            ("finished", "inspection complete"),
        ][step];
        Self {
            run_id: run.into(),
            event_id: format!("{agent}-{step}"),
            agent_id: agent.into(),
            actor: agent.into(),
            frame_id: format!("{agent}-frame"),
            caused_by: step
                .checked_sub(1)
                .map(|parent| format!("{agent}-{parent}")),
            kind: kind.into(),
            payload: payload.into(),
        }
    }

    fn key(&self) -> Bytes {
        Bytes::from(format!("{}/{}", self.run_id, self.agent_id))
    }
    fn record(&self) -> Result<Record> {
        Ok(Record {
            key: self.key(),
            value: Bytes::from(serde_json::to_vec(self)?),
        })
    }
}

#[derive(Default)]
struct Monitor {
    next: BTreeMap<String, u64>,
    events: BTreeMap<(String, String), TraceEvent>,
    duplicates: usize,
}

impl Monitor {
    async fn catch_up(&mut self, reader: &LogDbReader, run: &str, expected: usize) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                for agent in ["agent-a", "agent-b"] {
                    let key = format!("{run}/{agent}");
                    let from = *self.next.get(&key).unwrap_or(&0);
                    let mut scan = reader.scan(Bytes::from(key.clone()), from..).await?;
                    while let Some(entry) = scan.next().await? {
                        let event: TraceEvent = serde_json::from_slice(&entry.value)?;
                        if event.run_id != run || event.agent_id != agent {
                            return Err("event identity does not match its stream".into());
                        }
                        let identity = (event.run_id.clone(), event.event_id.clone());
                        if let Some(previous) = self.events.get(&identity) {
                            if previous != &event {
                                return Err("same source identity has conflicting payload".into());
                            }
                            self.duplicates += 1;
                            println!(
                                "  duplicate {} at storage sequence {}: already applied",
                                event.event_id, entry.sequence
                            );
                        } else {
                            if let Some(parent) = &event.caused_by
                                && !self.events.contains_key(&(run.to_owned(), parent.clone()))
                            {
                                return Err("causal parent missing from this trace".into());
                            }
                            println!(
                                "  [{}] {} {} (parent {:?})",
                                entry.sequence, agent, event.kind, event.caused_by
                            );
                            self.events.insert(identity, event);
                        }
                        // Only advance after validation and projection application succeed.
                        self.next.insert(
                            key.clone(),
                            entry.sequence.checked_add(1).ok_or("sequence overflow")?,
                        );
                    }
                }
                if self.events.len() == expected {
                    return Ok(());
                }
                if self.events.len() > expected {
                    return Err("unexpected extra source events".into());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(90), run()).await?
}

async fn run() -> Result<()> {
    let configuration = config("lesson-7")?;
    if matches!(
        configuration.storage,
        opendata_common::StorageConfig::InMemory
    ) {
        return Err("lesson 7 needs shared persistent storage for an independent reader".into());
    }
    let run = format!(
        "run-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let log = LogDb::open(configuration.clone()).await?;
    for agent in ["agent-a", "agent-b"] {
        for step in 0..2 {
            log.try_append(vec![TraceEvent::new(&run, agent, step).record()?])
                .await?;
        }
    }
    log.flush().await?;
    let reader = LogDbReader::open(reader_config(&configuration)).await?;
    let mut monitor = Monitor::default();
    println!("Initial connection: started and tool-request for both agents");
    monitor.catch_up(&reader, &run, 4).await?;
    reader.close().await;

    println!("Monitor disconnected; agents finish and retry one uncertain report");
    for agent in ["agent-a", "agent-b"] {
        for step in 2..4 {
            log.try_append(vec![TraceEvent::new(&run, agent, step).record()?])
                .await?;
        }
    }
    log.try_append(vec![TraceEvent::new(&run, "agent-a", 2).record()?])
        .await?;
    log.flush().await?;
    log.close().await?;

    let reader = LogDbReader::open(reader_config(&configuration)).await?;
    println!("Reconnect from saved per-stream positions:");
    monitor.catch_up(&reader, &run, 8).await?;
    if monitor.duplicates != 1 {
        return Err("expected exactly one retry duplicate".into());
    }
    println!("Rebuild projection from durable history (no saved positions):");
    let mut rebuilt = Monitor::default();
    rebuilt.catch_up(&reader, &run, 8).await?;
    if rebuilt.events != monitor.events || rebuilt.duplicates != 1 {
        return Err("full replay disagrees with resumed projection".into());
    }
    reader.close().await;
    println!("Verified: 9 stored records, 8 unique events, complete causal chains for 2 agents.");
    Ok(())
}
