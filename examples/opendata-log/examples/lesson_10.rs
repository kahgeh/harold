//! A bounded experiment: scheduled producers -> one durable writer -> independent reader.
use bytes::Bytes;
use opendata_agent_progress::{Result, config, reader_config};
use opendata_common::StorageConfig;
use opendata_log::{LogDb, LogDbReader, LogRead, Record};
use std::{
    env,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::mpsc,
    time::{sleep, sleep_until, timeout},
};

#[derive(Clone, serde::Serialize)]
struct Settings {
    producers: usize,
    events: usize,
    batch: usize,
    payload: usize,
    poll_ms: u64,
    refresh_ms: u64,
    rate: u64,
    timeout_s: u64,
    output: PathBuf,
}
impl Settings {
    fn load() -> Result<Self> {
        let settings = Self {
            producers: number("BENCH_PRODUCERS", 2)?,
            events: number("BENCH_EVENTS", 20)?,
            batch: number("BENCH_BATCH", 5)?,
            payload: number("BENCH_PAYLOAD", 256)?,
            poll_ms: number("BENCH_POLL_MS", 20)?,
            refresh_ms: number("BENCH_REFRESH_MS", 100)?,
            rate: number("BENCH_RATE", 100)?,
            timeout_s: number("BENCH_TIMEOUT_S", 60)?,
            output: env::var_os("BENCH_OUTPUT")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(format!(
                        "results/benchmark-{}.csv",
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_nanos())
                            .unwrap_or(0)
                    ))
                }),
        };
        if !(1..=100).contains(&settings.producers)
            || settings.events == 0
            || settings.batch == 0
            || !settings.events.is_multiple_of(settings.batch)
            || settings.batch > 1000
            || !(16..=8192).contains(&settings.payload)
            || !(1..=60_000).contains(&settings.poll_ms)
            || !(1..=60_000).contains(&settings.refresh_ms)
            || settings.rate == 0
            || !(1..=600).contains(&settings.timeout_s)
            || settings
                .producers
                .checked_mul(settings.events)
                .is_none_or(|n| {
                    n > 100_000
                        || n.checked_mul(settings.payload)
                            .is_none_or(|bytes| bytes > 64 * 1024 * 1024)
                })
        {
            return Err("Use producers 1..100, payload 16..8192, positive rate, poll/refresh 1..60000ms, timeout 1..600s, events divisible by batch, batch <=1000, <=100000 total events and <=64MiB total payload".into());
        }
        Ok(settings)
    }
    fn total(&self) -> usize {
        self.producers * self.events
    }
}
fn number<T: std::str::FromStr>(name: &str, default: T) -> Result<T> {
    match env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|_| format!("Invalid {name}: {value}").into()),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}
fn key(producer: usize) -> Bytes {
    Bytes::from(format!("producer-{producer}"))
}
fn payload(producer: usize, event: usize, size: usize) -> Bytes {
    let mut bytes = vec![b'x'; size];
    bytes[..8].copy_from_slice(&(producer as u64).to_be_bytes());
    bytes[8..16].copy_from_slice(&(event as u64).to_be_bytes());
    Bytes::from(bytes)
}
struct Batch {
    producer: usize,
    first: usize,
    scheduled: Duration,
    records: Vec<Record>,
}
#[derive(Clone, Default)]
struct Sample {
    scheduled: Duration,
    append_start: Duration,
    accepted: Option<Duration>,
    durable: Option<Duration>,
    sequence: Option<u64>,
    queued_batches_after_dequeue: usize,
}
struct Writes {
    samples: Vec<Option<Sample>>,
    error: Option<String>,
    finished: Duration,
    max_sampled_queued_batches: usize,
}
struct Reads {
    observed: Vec<Option<(u64, Duration)>>,
    error: Option<String>,
    polls: usize,
}

async fn produce(
    settings: Settings,
    producer: usize,
    start: Instant,
    sender: mpsc::Sender<Batch>,
) -> Result<()> {
    for first in (0..settings.events).step_by(settings.batch) {
        // Interleave each producer's batches on a single aggregate offered-rate timeline.
        let ordinal = (first / settings.batch * settings.producers + producer) * settings.batch;
        let scheduled = Duration::from_secs_f64(ordinal as f64 / settings.rate as f64);
        sleep_until((start + scheduled).into()).await;
        let records = (first..first + settings.batch)
            .map(|event| Record {
                key: key(producer),
                value: payload(producer, event, settings.payload),
            })
            .collect();
        sender
            .send(Batch {
                producer,
                first,
                scheduled,
                records,
            })
            .await
            .map_err(|_| "writer stopped")?;
    }
    Ok(())
}
async fn write_batches(
    log: &LogDb,
    settings: &Settings,
    start: Instant,
    mut receiver: mpsc::Receiver<Batch>,
) -> Writes {
    let mut result = Writes {
        samples: vec![None; settings.total()],
        error: None,
        finished: Duration::ZERO,
        max_sampled_queued_batches: 0,
    };
    let operation = async {
        while let Some(batch) = receiver.recv().await {
            let append_start = start.elapsed();
            let queued_batches_after_dequeue = receiver.len();
            result.max_sampled_queued_batches = result
                .max_sampled_queued_batches
                .max(queued_batches_after_dequeue);
            let indices = batch.first..batch.first + settings.batch;
            for event in indices.clone() {
                result.samples[batch.producer * settings.events + event] = Some(Sample {
                    scheduled: batch.scheduled,
                    append_start,
                    queued_batches_after_dequeue,
                    ..Sample::default()
                });
            }
            let appended = log.try_append(batch.records).await?;
            let accepted = start.elapsed();
            for (offset, event) in indices.clone().enumerate() {
                if let Some(sample) = &mut result.samples[batch.producer * settings.events + event]
                {
                    sample.accepted = Some(accepted);
                    sample.sequence = Some(appended.start_sequence + offset as u64);
                }
            }
            log.flush().await?;
            let durable = start.elapsed();
            for event in indices {
                if let Some(sample) = &mut result.samples[batch.producer * settings.events + event]
                {
                    sample.durable = Some(durable);
                }
            }
        }
        Ok::<(), opendata_log::Error>(())
    };
    result.error = match timeout(Duration::from_secs(settings.timeout_s), operation).await {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error.to_string()),
        Err(_) => Some(
            "writer workload deadline exceeded; last append/flush outcome may be uncertain".into(),
        ),
    };
    result.finished = start.elapsed();
    result
}
async fn observe(reader: &LogDbReader, settings: &Settings, start: Instant) -> Reads {
    let mut result = Reads {
        observed: vec![None; settings.total()],
        error: None,
        polls: 0,
    };
    let operation = async {
        let mut next = vec![0; settings.producers];
        let mut count = 0;
        while count < settings.total() {
            result.polls += 1;
            for (producer, cursor) in next.iter_mut().enumerate() {
                let mut scan = reader.scan(key(producer), *cursor..).await?;
                while let Some(entry) = scan.next().await? {
                    if entry.value.len() != settings.payload {
                        return Err("unexpected payload size".into());
                    }
                    let event = u64::from_be_bytes(entry.value[8..16].try_into()?) as usize;
                    if event >= settings.events
                        || entry.value != payload(producer, event, settings.payload)
                    {
                        return Err("unknown or corrupted event".into());
                    }
                    let index = producer * settings.events + event;
                    if result.observed[index].is_some() {
                        return Err("duplicate event identity".into());
                    }
                    if event > 0 && result.observed[index - 1].is_none() {
                        return Err("per-producer event order violated".into());
                    }
                    if entry.sequence < *cursor {
                        return Err("storage sequence regressed".into());
                    }
                    result.observed[index] = Some((entry.sequence, start.elapsed()));
                    *cursor = entry.sequence.checked_add(1).ok_or("sequence overflow")?;
                    count += 1;
                }
            }
            if count < settings.total() {
                sleep(Duration::from_millis(settings.poll_ms)).await;
            }
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    };
    result.error = match timeout(Duration::from_secs(settings.timeout_s), operation).await {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error.to_string()),
        Err(_) => Some("reader visibility deadline exceeded".into()),
    };
    result
}
fn micros(value: Option<Duration>) -> String {
    value.map(|v| v.as_micros().to_string()).unwrap_or_default()
}
fn write_csv(
    file: &mut std::fs::File,
    settings: &Settings,
    writes: &Writes,
    reads: &Reads,
) -> Result<()> {
    writeln!(
        file,
        "producer,event,scheduled_us,append_start_us,accepted_us,durable_us,visible_us,sequence,queued_batches_after_dequeue"
    )?;
    for index in 0..settings.total() {
        let sample = writes.samples[index].clone().unwrap_or_default();
        writeln!(
            file,
            "{},{},{},{},{},{},{},{},{}",
            index / settings.events,
            index % settings.events,
            if writes.samples[index].is_some() {
                micros(Some(sample.scheduled))
            } else {
                String::new()
            },
            if writes.samples[index].is_some() {
                micros(Some(sample.append_start))
            } else {
                String::new()
            },
            micros(sample.accepted),
            micros(sample.durable),
            micros(reads.observed[index].map(|(_, time)| time)),
            sample.sequence.map(|n| n.to_string()).unwrap_or_default(),
            if writes.samples[index].is_some() {
                sample.queued_batches_after_dequeue.to_string()
            } else {
                String::new()
            }
        )?;
    }
    file.sync_all()?;
    Ok(())
}
fn distribution(name: &str, mut values: Vec<Duration>) {
    if values.is_empty() {
        println!("{name}: no samples");
        return;
    }
    values.sort_unstable();
    let percentile = |percent: usize| {
        values[(values.len() * percent).div_ceil(100).saturating_sub(1)].as_secs_f64() * 1000.0
    };
    println!(
        "{name}: n={} p50={:.3}ms p95={:.3}ms p99={:.3}ms max={:.3}ms",
        values.len(),
        percentile(50),
        percentile(95),
        percentile(99),
        percentile(100)
    );
}
fn summarize(settings: &Settings, writes: &Writes, reads: &Reads) -> Result<()> {
    let mut accepted = Vec::new();
    let mut queued = Vec::new();
    let mut durable = Vec::new();
    let mut flush = Vec::new();
    let mut visible = Vec::new();
    let mut missing = 0;
    for index in 0..settings.total() {
        let Some(sample) = &writes.samples[index] else {
            missing += 1;
            continue;
        };
        queued.push(sample.append_start.saturating_sub(sample.scheduled));
        if let Some(time) = sample.accepted {
            accepted.push(time.saturating_sub(sample.append_start));
        }
        if let Some(time) = sample.durable {
            durable.push(time.saturating_sub(sample.scheduled));
            if let Some(accepted) = sample.accepted {
                flush.push(time.saturating_sub(accepted));
            }
        }
        match reads.observed[index] {
            Some((sequence, time)) if Some(sequence) == sample.sequence => {
                visible.push(time.saturating_sub(sample.scheduled))
            }
            Some(_) => return Err("reader/writer sequence mismatch".into()),
            None => missing += 1,
        }
    }
    distribution(
        "Scheduled arrival -> append start (queue/scheduling)",
        queued,
    );
    distribution("Append call -> acceptance", accepted);
    distribution("Acceptance -> flush completion", flush);
    let durable_count = durable.len();
    distribution("Scheduled arrival -> durable acknowledgment", durable);
    distribution(
        "Scheduled arrival -> independent reader observation",
        visible,
    );
    println!(
        "Offered target={} events/s; durable completion rate={:.2} events/s; durable={}/{}; missing observations={}; reader polls={}; operation errors={}",
        settings.rate,
        durable_count as f64 / writes.finished.as_secs_f64(),
        durable_count,
        settings.total(),
        missing,
        reads.polls,
        usize::from(writes.error.is_some()) + usize::from(reads.error.is_some())
    );
    println!(
        "Durable logical payload rate={:.2} bytes/s; max sampled queued batches after dequeue={} (channel capacity={}; not true high-water mark)",
        (durable_count * settings.payload) as f64 / writes.finished.as_secs_f64(),
        writes.max_sampled_queued_batches,
        settings.producers * 2
    );
    if let Some(error) = &writes.error {
        eprintln!("Writer: {error}");
    }
    if let Some(error) = &reads.error {
        eprintln!("Reader: {error}");
    }
    if missing != 0
        || durable_count != settings.total()
        || writes.error.is_some()
        || reads.error.is_some()
    {
        return Err(
            "experiment failed; retain raw samples, do not treat this as a successful benchmark"
                .into(),
        );
    }
    println!(
        "Validated every event's payload, identity, sequence and producer order; no duplicates."
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let settings = Settings::load()?;
    if let Some(parent) = settings
        .output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    // Reserve output before creating a database: never overwrite a previous experiment.
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&settings.output)?;
    let namespace = format!(
        "lesson-10-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let mut cfg = config(&namespace)?;
    // OPENDATA_PREFIX is a parent prefix here: every benchmark gets a new database.
    if let StorageConfig::SlateDb(storage) = &mut cfg.storage
        && storage.path != namespace
    {
        storage.path = format!("{}/{namespace}", storage.path);
    }
    if matches!(cfg.storage, StorageConfig::InMemory) {
        return Err(
            "Lesson 10 requires local or S3 storage: independent InMemory opens do not share data"
                .into(),
        );
    }
    let mut reader_cfg = reader_config(&cfg);
    reader_cfg.refresh_interval = Duration::from_millis(settings.refresh_ms);
    let metadata_path = settings.output.with_extension("metadata.json");
    let mut metadata = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&metadata_path)?;
    serde_json::to_writer_pretty(
        &mut metadata,
        &serde_json::json!({
            "settings": settings, "namespace": namespace, "storage": format!("{:?}", cfg.storage),
            "reader_refresh_ms": reader_cfg.refresh_interval.as_millis(), "opendata_log": "1.0.0",
        "writer_config": cfg,
            "clock": "single-process monotonic Instant", "mode": "fresh prefix; no warmup; flush per batch",
            "cargo_lock": include_str!("../Cargo.lock"),
            "limitations": "CPU and peak memory require scripts/benchmark.py. No S3 request/network-byte/cost or cross-host timing collection. No power-loss test."
        }),
    )?;
    metadata.sync_all()?;
    let deadline = Duration::from_secs(settings.timeout_s);
    let opening = Instant::now();
    let log = timeout(deadline, LogDb::open(cfg.clone())).await??;
    println!(
        "Fresh writer open: {:.3}ms",
        opening.elapsed().as_secs_f64() * 1000.0
    );
    timeout(deadline, log.flush()).await??;
    let reader = timeout(deadline, LogDbReader::open(reader_cfg.clone())).await??;
    let start = Instant::now();
    let (sender, receiver) = mpsc::channel(settings.producers * 2);
    let mut producers = Vec::new();
    for producer in 0..settings.producers {
        let settings = settings.clone();
        let sender = sender.clone();
        producers.push(tokio::spawn(produce(settings, producer, start, sender)));
    }
    drop(sender);
    let (writes, reads) = tokio::join!(
        write_batches(&log, &settings, start, receiver),
        observe(&reader, &settings, start)
    );
    for producer in producers {
        producer.abort();
    }
    write_csv(&mut output, &settings, &writes, &reads)?;
    println!(
        "Raw samples: {}; metadata: {}",
        settings.output.display(),
        metadata_path.display()
    );
    let outcome = summarize(&settings, &writes, &reads);
    timeout(deadline, reader.close()).await?;
    timeout(deadline, log.close()).await??;
    outcome?;

    // Reconnect after completion and read the entire durable history from a fresh reader.
    let catchup_start = Instant::now();
    let catchup_reader = timeout(deadline, LogDbReader::open(reader_cfg)).await??;
    let catchup = observe(&catchup_reader, &settings, catchup_start).await;
    let catchup_elapsed = catchup_start.elapsed();
    timeout(deadline, catchup_reader.close()).await?;
    if let Some(error) = catchup.error {
        return Err(format!("Catchup failed: {error}").into());
    }
    for (index, observed) in catchup.observed.iter().enumerate() {
        if observed.map(|(sequence, _)| sequence)
            != writes.samples[index].as_ref().and_then(|s| s.sequence)
        {
            return Err("catchup sequence mismatch".into());
        }
    }
    println!(
        "Reader open + catchup of {} events: {:.3}ms",
        settings.total(),
        catchup_elapsed.as_secs_f64() * 1000.0
    );
    let reopen_start = Instant::now();
    let reopened = timeout(deadline, LogDb::open(cfg)).await??;
    println!(
        "Writer reopen after clean close: {:.3}ms (not failure detection or crash takeover)",
        reopen_start.elapsed().as_secs_f64() * 1000.0
    );
    timeout(deadline, reopened.close()).await??;
    Ok(())
}
