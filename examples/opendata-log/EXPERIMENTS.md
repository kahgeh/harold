# Streaming, storage, and performance experiments

These experiments answer a practical question: can OpenData reliably carry
agent execution traces, and what latency and throughput can we measure for
each storage option?

Status: the numbered lessons now provide local/cloud configuration, simulated
trace replay, a controlled process-failure runner, and a bounded benchmark. See
[verification results](VERIFICATION.md) for executed checks. This document also
defines the larger experimental matrix: a runnable harness is not evidence that
every backend, workload size, credential-renewal interval or failure was tested.

## Keep the workload understandable

Start with deterministic simulated agents emitting `started`, tool-request,
tool-result, and `finished` events. Preserve a stable source identity such as
`(run_id, event_id)`, an explicit agent identity, and causal parent references.
Keep source event identity separate from OpenData's storage sequence number.

Use ActiveGraph's accepted execution events as the reference model. Its live
EventSink can drop events under pressure, so a later real integration must
support catch-up from durable source history. A fast live feed alone cannot
prove complete capture. Token streaming is a separate feature from execution
trace streaming.

The workload may use synthetic user/session labels to test stream separation.
User accounts, access control, workflow extraction, refinement, and execution
belong to the separate application repository.

## Storage comparisons

| Backend | Purpose | Placement to record |
| --- | --- | --- |
| In-memory | Understand API and processing overhead | Local machine |
| Local filesystem | Restart/replay baseline | Filesystem, device, and machine |
| S3 Standard | Object storage baseline | Bucket region and client location |
| S3 Express One Zone | Directory-bucket compatibility and latency experiment | Bucket zone ID, client zone ID, and region |

Use the same workload and acknowledgment contract for comparisons. Report
the storage backends separately; a laptop-to-AWS run and a same-zone compute
run answer different questions. Do not label a backend faster based on
different batch sizes, client placement, or durability settings.

S3 Express integration must be verified with the pinned OpenData and
object-store versions before claiming cloud support. The updated [dependency audit](DEPENDENCIES.md) permits bounded official-AWS
experiments and records the remaining XML parser vulnerabilities.

### S3 Express readiness

Source inspection found a configuration route in the current lockfile:
`opendata-common` 0.1.17 builds its AWS backend with
`AmazonS3Builder::from_env()`, plus the configured region and bucket.
`object_store` 0.12.5 recognizes `AWS_S3_EXPRESS=true`, derives a zonal endpoint
from the full directory-bucket name, and uses a refreshing session credential
provider. No new dependency appears necessary for this configuration path.
This is source evidence, not a successful integration run.

The lesson requires Express mode in its process environment, plus the
region and directory-bucket name, and let the builder derive the endpoint.
Use base AWS credentials accepted by this Rust provider; do not assume a
successful CLI login proves that the application can authenticate. Keep
conditional writes enabled so SlateDB can fence older writers.

AWS requires directory buckets for Express, with `s3express:CreateSession`
authorization for session-based object access. Verify append, flush, reopen,
listing/recovery, independent-reader catch-up, fencing, and credential refresh
in a run long enough to exercise session renewal.
[AWS authorization documentation](https://docs.aws.amazon.com/AmazonS3/latest/userguide/s3-express-authenticating-authorizing.html).

For the performance comparison, run both cloud backends from the same compute
instance in the Express bucket's availability zone. Keep local Mac-to-AWS
measurements as a separate integration/WAN scenario.
[AWS performance guidance](https://docs.aws.amazon.com/AmazonS3/latest/userguide/s3-express-optimizing-performance-design-patterns.html).

Express stores redundant copies within one availability zone; it does not
provide S3 Standard's multi-zone failure boundary. A successful writer-process
failover test does not establish survival of a storage-zone failure. Compare
latency alongside that difference, and distinguish advertised object-request
latency from measured durable-event and recovery latency.
[AWS reliability discussion](https://docs.aws.amazon.com/solutions/writing-high-transaction-workloads-on-amazon-s3-express-one-zone/).

## Reliability before speed

The reliability harness retains a record of source event IDs and which
acknowledgments it observed, outside the process being terminated. After
reopening the log, reconcile recovered events against that record.

| Experiment | What to demonstrate |
| --- | --- |
| Graceful restart | Durable events reappear and the reader resumes from its saved position. |
| Crash after buffered acceptance | Report which unconfirmed records survived; do not count acceptance as durability. |
| Crash after durable acknowledgment | Every durably acknowledged event remains recoverable, with unchanged payload. |
| Lost reply and retry | A stored write may have an unknown outcome to its producer; measure duplicate source IDs and validate the chosen consumer deduplication policy. |
| Slow/disconnected reader | Backlog grows visibly; reconnect and catch up without silent omissions. |
| Concurrent producers | Preserve the database's accepted order per stream; do not infer source-time or cross-machine causal order from arrival alone. |
| Writer takeover | Pause A, promote B, resume A, and verify A is fenced and B can durably append. |
| Transient storage failure | Surface errors/backpressure, bound queued work, and reconcile recovery rather than silently dropping reports. |

Use a dedicated database path/prefix for each trial. Avoid reusing old traces
or old checkpoints as the initial state of a supposedly fresh run. Record
retention settings so removed history is not mistaken for failed durability.
Local filesystem results are not proof of machine-power-loss safety.

## Measure distinct intervals

| Measurement | Start | End |
| --- | --- | --- |
| Append acceptance | Producer submits a report | Append API returns |
| Durable acknowledgment | Producer submits a report | Storage confirms that report is durable |
| Reader visibility | Producer submits a report | An independent reader observes its source ID |
| Catch-up | Disconnected reader reconnects | Reader reaches the selected durable watermark |
| Writer open | Replacement starts opening the database | Open/recovery returns successfully |
| End-to-end failover | Harness injects the writer failure | A client receives the first durable acknowledgment through the replacement |

Measure with monotonic clocks in a single timing coordinator where possible.
If timestamps cross machines, state the synchronization method and its error.
Record reader polling intervals: a 100 ms polling delay must not be presented
as 100 ms of storage latency. Separate independent-reader results from reads
through the writer handle.

Break failover into detection, selection, fencing/recovery, routing/retry,
and final durable-write time. A quick writer-open measurement is not a bound
on the interruption clients experience.

## Workload matrix

Begin with a small sweep, expanding only when a result needs explanation:

- Producers: 1, 10, 100 simulated agents.
- Payload: 256 bytes, 1 KiB, 8 KiB.
- Batch size: 1, 10, 100 records.
- Readers: one independent monitor, then several.
- Arrival rate: increase a controlled offered rate until backlog grows or
  errors appear; also measure a producer that waits for each acknowledgment.
- Storage state: fresh database, warm reads, restart with a recorded recovery
  backlog, and a longer run that includes flush/compaction work.

Record offered and completed rates separately. Measure queue waiting from
the intended send time so an overloaded producer does not hide latency by
silently sending fewer requests. Bound load generation and report any work it
could not submit. Run warm-up and repeated trials with explicit durations.

## Evidence to keep

Each result should include:

- Exact application revision, Cargo.lock identity, backend configuration,
  durability/read-visibility settings, and polling/batching policy.
- Machine/instance type, client and bucket placement, object-store endpoint
  class, and cold/warm state. Exclude credentials.
- Sample count, p50/p95/p99 latency, maximum latency, successful durable
  records/second, and bytes/second. Add higher percentiles only with enough
  samples to make them meaningful.
- Error counts, timeouts, duplicate source IDs, missing acknowledged IDs,
  ordering violations, reader lag, queue depth, CPU, and memory.
- S3 request counts and bytes transferred. If estimating cost, record the
  region, pricing date, and source rather than assuming requests are free.
- Failure injection, outstanding recovery backlog, and time to recovery.

Keep raw measurements and a short interpretation. Distinguish observed
results from targets; no latency or failover guarantee is established yet.

## Reading order

Return to the [lesson roadmap](README.md#where-we-go-next). Storage and trace
lessons build the mechanisms that these experiments will exercise.
