//! A small JSON-lines process controlled by scripts/reliability.py.
use bytes::Bytes;
use opendata_agent_progress::{Result, config, reader_config};
use opendata_log::{LogDb, LogDbReader, LogRead, Record};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Command {
    Append {
        id: String,
        body: String,
        durable: bool,
        deadline_ms: Option<u64>,
    },
    Flush,
    Scan {
        from: u64,
    },
    Close,
}

fn reply(value: &Value) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, value)?;
    writeln!(output)?;
    output.flush()?;
    Ok(())
}

async fn scan(log: &(impl LogRead + Sync), from: u64) -> Result<Value> {
    let mut iterator = log.scan(Bytes::from_static(b"agent-a"), from..).await?;
    let mut records = Vec::new();
    while let Some(record) = iterator.next().await? {
        records.push(json!({"sequence": record.sequence,
            "event": serde_json::from_slice::<Value>(&record.value)?}));
    }
    Ok(json!({"records": records}))
}

async fn execute(log: &LogDb, command: Command) -> Result<Value> {
    match command {
        Command::Append {
            id,
            body,
            durable,
            deadline_ms,
        } => {
            let append = async {
                let result = log
                    .try_append(vec![Record {
                        key: Bytes::from_static(b"agent-a"),
                        value: Bytes::from(serde_json::to_vec(&json!({"id": id, "body": body}))?),
                    }])
                    .await?;
                if durable {
                    log.flush().await?;
                }
                Ok(json!({"sequence": result.start_sequence, "durable": durable}))
            };
            match deadline_ms {
                Some(ms) => tokio::time::timeout(Duration::from_millis(ms), append)
                    .await
                    .map_err(|_| "application durability deadline expired; outcome unknown")?,
                None => append.await,
            }
        }
        Command::Flush => {
            log.flush().await?;
            Ok(json!({}))
        }
        Command::Scan { from } => scan(log, from).await,
        Command::Close => unreachable!("close is handled by the owner"),
    }
}

fn report(result: Result<Value>, started: Instant) -> Result<()> {
    match result {
        Ok(mut value) => {
            value["ok"] = json!(true);
            value["elapsed_us"] = json!(started.elapsed().as_micros());
            reply(&value)
        }
        Err(error) => reply(&json!({"ok": false, "error": error.to_string(),
            "elapsed_us": started.elapsed().as_micros()})),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("writer");
    let namespace = args.get(2).map(String::as_str).unwrap_or("lesson-9");
    let settings = config(namespace)?;
    let started = Instant::now();
    if mode == "reader" {
        let reader = LogDbReader::open(reader_config(&settings)).await?;
        reply(&json!({"ok": true, "ready": "reader", "open_us": started.elapsed().as_micros()}))?;
        for line in io::stdin().lock().lines() {
            let command: Command = serde_json::from_str(&line?)?;
            let started = Instant::now();
            match command {
                Command::Scan { from } => report(scan(&reader, from).await, started)?,
                Command::Close => break,
                _ => reply(&json!({"ok": false, "error": "reader only supports scan and close"}))?,
            }
        }
        reader.close().await;
    } else if mode == "writer" {
        let log = LogDb::open(settings).await?;
        reply(&json!({"ok": true, "ready": "writer", "open_us": started.elapsed().as_micros()}))?;
        for line in io::stdin().lock().lines() {
            let command: Command = serde_json::from_str(&line?)?;
            if matches!(command, Command::Close) {
                break;
            }
            let started = Instant::now();
            report(execute(&log, command).await, started)?;
        }
        log.close().await?;
    } else {
        return Err("mode must be writer or reader".into());
    }
    Ok(())
}
