# Lesson 10: measure the whole route to a monitor

Two agents each produce 20 reports. Their reports wait in a bounded queue, one writer appends and flushes them, and an independent reader polls the persisted database. The experiment records when each report passes each point. A fast `try_append` alone does not tell you how quickly a monitor can see a durable report.

From this crate's directory:

```sh
cargo run --locked --offline --example lesson_10
```

This small local run prints latency percentiles, validates all 40 reports, reconnects a reader to replay the history, and closes/reopens the writer. It creates a fresh database beneath `.data/lesson-10-<pid>-<time>/`, plus `results/benchmark-<time>.csv` and a matching `.metadata.json` beneath your current directory. If `OPENDATA_PREFIX` is set, lesson 10 appends its unique namespace beneath that prefix. Each experiment therefore starts with its own database. It never deletes results or database objects.

The independent reader is a separately opened `LogDbReader`, using the same persistent backend. This measures independent storage visibility within one process, without an HTTP transport or a second host. `OPENDATA_BACKEND=memory` is rejected: separate in-memory opens would have different stores.

## Read one row before comparing percentiles

The CSV timestamps are microseconds relative to one monotonic clock. Each row identifies a producer and that producer's event number. Read a row as:

1. `scheduled_us`: when this batch was supposed to arrive.
2. `append_start_us`: when the writer actually called `try_append`.
3. `accepted_us`: when `try_append` returned its sequence assignment.
4. `durable_us`: when the explicit `flush` completed.
5. `visible_us`: when the independent reader observed this report.

`queued_batches_after_dequeue` records the channel length sampled immediately after the writer dequeues this report's batch. The summary prints its maximum. This is a sampled queue backlog, not the true high-water mark: arrivals between samples, the active batch, and producers blocked while sending are not counted. Reports in a batch repeat the same sample. Compare it with the scheduling/queue wait to see congestion.

`append_start - scheduled` includes producer scheduling, bounded-channel waiting and waiting behind previous writes. `accepted - append_start` isolates the append call. `durable - accepted` measures the explicit flush wait. `durable - scheduled` and `visible - scheduled` include delays caused by overload. This prevents a slow producer or queue from disappearing from the reported end-to-end latency.

A batch gets one scheduled arrival and one acceptance/flush timestamp. Its reports consequently share those samples. Percentiles are **event-weighted, correlated batch observations**, not independent requests. Batches from different producers are scheduled in round-robin slots on the aggregate offered-rate timeline. This is a batched, paced workload, not a model of uniformly spaced individual report arrivals.

The reader polls every 20 ms by default. Its internal manifest refresh interval defaults to 100 ms (`BENCH_REFRESH_MS`), recorded in metadata. Observation time includes both waits and scanning: it is an upper bound on when storage became readable. An independent reader may observe an event before the writer's `flush` future returns, so visibility is measured from scheduled arrival, never by subtracting the durable callback timestamp.

The final two timings measure reader **open plus complete replay**, and writer **open after a clean close**. Neither is crash recovery or failure detection. Lesson 9 tests failure behavior.

## Change one variable

Settings are read once at startup:

| Environment variable | Default | Meaning |
| --- | ---: | --- |
| `BENCH_PRODUCERS` | 2 | Concurrent producers, 1–100 |
| `BENCH_EVENTS` | 20 | Reports per producer; positive multiple of batch size |
| `BENCH_BATCH` | 5 | Reports per append and explicit flush, 1–1000 |
| `BENCH_PAYLOAD` | 256 | Bytes per report including its 16-byte identity, 16–8192 |
| `BENCH_RATE` | 100 | Target aggregate offered reports/second, positive |
| `BENCH_POLL_MS` | 20 | Monitor scan interval, 1–60000 ms |
| `BENCH_REFRESH_MS` | 100 | Reader manifest refresh interval, 1–60000 ms |
| `BENCH_TIMEOUT_S` | 60 | Deadline for each workload phase or open/close operation, 1–600 seconds |
| `BENCH_OUTPUT` | unique name under `results/` | Output file; must not already exist |

Total work is capped at 100,000 reports and 64 MiB of logical payload. The channel holds at most twice the producer count in batches; each producer can hold one additional batch while blocked. The database and sample arrays also consume memory. Settings are deliberately small enough to inspect and rerun; this is not an unbounded load generator.

For more useful numbers, build release mode once and time the already built program:

```sh
cargo build --locked --offline --release --example lesson_10
BENCH_PRODUCERS=10 BENCH_EVENTS=100 BENCH_BATCH=10 \
  BENCH_RATE=1000 BENCH_OUTPUT=local-10-producers.csv \
  target/release/examples/lesson_10 > local-10-producers.txt 2>&1
```

The summary reports the requested offered rate, actual durable completions and logical payload bytes divided by elapsed workload time, plus p50/p95/p99/max using the nearest-rank method. Very short runs can finish above the target rate because the first batch arrives at time zero and no idle final interval is added. Longer runs reduce that edge effect. Forty events cannot establish a reliable p99; increase the bounded workload and run repetitions before comparing tails.

The experiment checks identity, exact payload, per-producer order, reader/writer sequence agreement, missing events, and duplicates. On a write/read deadline or error, it keeps partial CSV data, prints the error and missing counts, and exits unsuccessfully. Empty cells mean a stage was not confirmed. A timed-out append/flush can have an uncertain outcome; incomplete data must not be presented as a successful latency result. Startup or filesystem errors can occur before samples exist. Reader catchup also validates identities and sequence agreement with the original writes.

## Compare S3 Standard and Express

Complete lessons 5 and 8 first, using an existing dedicated test bucket and credentials. The benchmark creates objects and leaves them for inspection and explicit cleanup. Use precisely the same release executable, settings, machine, region and repetitions for both backends:

```sh
OPENDATA_BACKEND=s3 AWS_REGION=YOUR_REGION AWS_BUCKET=YOUR_STANDARD_BUCKET \
  AWS_S3_EXPRESS=false OPENDATA_PREFIX=opendata-lessons/performance \
  BENCH_EVENTS=100 BENCH_BATCH=10 BENCH_RATE=100 \
  BENCH_OUTPUT=standard-01.csv target/release/examples/lesson_10 > standard-01.txt 2>&1

OPENDATA_BACKEND=express AWS_REGION=YOUR_REGION AWS_BUCKET=YOUR_DIRECTORY_BUCKET \
  AWS_S3_EXPRESS=true OPENDATA_PREFIX=opendata-lessons/performance \
  BENCH_EVENTS=100 BENCH_BATCH=10 BENCH_RATE=100 \
  BENCH_OUTPUT=express-01.csv target/release/examples/lesson_10 > express-01.txt 2>&1
```

Use the same `BENCH_REFRESH_MS` and `BENCH_POLL_MS` for both backends. A 100 ms refresh can dominate the difference between fast object stores; lower it explicitly when exploring storage latency. More frequent refreshes can increase object-store requests and cost.

Run both from the same EC2 machine in the directory bucket's Availability Zone when testing Express locality. A Mac-to-AWS run measures that network route too; label it separately. Alternate backends over several fresh trials, then vary one of producer count, payload, batch size or offered rate. Run with a fresh output filename every time. Confirm each process exits successfully before collecting its summary.

These are **fresh-prefix trials, with no warmup exclusion**. First writes, credentials and connections may be cold, while the operating system or service caches may already be warm; this harness does not purge caches or claim a controlled cold-cache experiment. It measures one independent reader; a multi-reader or long-running compaction workload would require a separate extension.

## Measure process resources

Run the standard-library Python wrapper against an already built binary:

```sh
BENCH_PRODUCERS=10 BENCH_EVENTS=100 BENCH_BATCH=10 BENCH_RATE=1000 \
  python3 scripts/benchmark.py --binary target/release/examples/lesson_10 \
  --output results/local-resources-01.json
```

The wrapper builds nothing and starts one child. It saves the child's stdout/stderr beside the resource JSON as `.stdout.txt`. By default, the child writes the matching `.csv` and `.metadata.json`; set `BENCH_OUTPUT` to choose a separate CSV path. All output paths must be fresh. The JSON records monotonic elapsed time, exit status, user/system CPU seconds and peak resident memory across all child threads. `getrusage(RUSAGE_CHILDREN)` reports peak RSS in bytes on macOS and KiB on Linux; the wrapper normalizes to bytes. The process measurement includes startup, writing, readers, catchup and closing, and excludes the Python wrapper. It is not a time series or per-phase resource attribution.

`--timeout 600` defaults to a ten-minute wall-time limit for the entire child. On timeout or interruption, the wrapper kills and waits for only its own child, records failure and exits unsuccessfully. A killed child may not have written complete CSV samples; retain its transcript and failure JSON. The inner benchmark's phase deadlines still apply. A wrapper success means its child exited successfully, not that a particular throughput target was met.

Metadata saves the workload settings, resolved writer/storage configuration, reader refresh period and the exact Cargo lockfile. Alongside the output, record `git rev-parse HEAD`, `git status --short`, `rustc --version`, OS, CPU/memory, region/AZ, instance type, timestamps and whether the executable was built in release mode. Include a source diff for uncommitted example code. The wrapper supplies process CPU/peak memory; collect time-series metrics externally if needed. Neither program measures S3 request counts, network bytes or cost. Logical payload bytes/second excludes storage formats, compaction and network overhead. This lesson supplies a repeatable experiment, not an AWS performance claim.
