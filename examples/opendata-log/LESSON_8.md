# Lesson 8: repeat recovery on S3 Express One Zone

Use an existing S3 Express One Zone **directory bucket**. Its full name
includes the availability-zone identifier and ends in `--x-s3`.
A general purpose bucket is not interchangeable with a directory bucket.

```sh
export OPENDATA_BACKEND=express
export AWS_S3_EXPRESS=true
export AWS_REGION=ap-southeast-2
export AWS_BUCKET=your-bucket--apse2-az1--x-s3
export OPENDATA_PREFIX=opendata-lab/your-unique-trial/lesson-8
export OPENDATA_CHECKPOINT=.checkpoints/your-unique-trial-lesson-8.json
cargo run --locked --offline --example lesson_8
cargo run --locked --offline --example lesson_8
```

Replace the bucket, region, zone, and trial identifiers with your actual
values. These commands do not create resources. Credentials must permit
`s3express:CreateSession` for the directory bucket as well as the applicable
S3 operations. The pinned `object_store` client uses the Express flag to
select the directory-bucket endpoint and acquire/refresh session
credentials. The Rust process reads the flag at startup; the examples do
not modify process environment from concurrent Rust code.

This entry point rejects local/S3 Standard backends. It runs the very same
flush, close, reopen, replay, and checkpoint assertions as lessons 4 and 5.
No successful local fallback is reported as an Express result. A short
successful run tests session acquisition; it does **not** establish that
credential refresh works across an expiry boundary. Leave an authorized
long-running workload in lesson 10 running across that boundary to test it.
Missing credentials, wrong region, and permissions errors must surface as
errors, not empty successful scans.

Use a unique prefix per trial. It holds an entire database, so opening a
second writer there takes writer ownership. The local checkpoint still
needs its own path and is not stored in the directory bucket. To reset,
stop the experiment, remove only its chosen object prefix, and remove its
local checkpoint. Cloud requests and stored data incur costs.

For a meaningful Standard/Express comparison, use the same EC2 instance,
payloads, rate, batching, readers, and software versions. Put compute in the
Express bucket's availability zone and record the AZ ID (AZ names can vary
between accounts). A Mac run is useful for correctness but includes WAN
latency. S3 Express stores data within one availability zone; it does not
have S3 Standard's multi-AZ failure envelope. Writer process recovery and
AZ-loss resilience are different questions.

The defaults preserve object-store conditional writes, which SlateDB uses
for writer fencing. Endpoint, TLS, and conditional-put overrides are
rejected. Do not disable those protections to make a failing experiment
pass. The dependency audit's XML parser residual risk is documented in
[DEPENDENCIES.md](DEPENDENCIES.md); a successful run does not remove it.

For a bounded, roughly 6-minute session-refresh trial, keep the Express
configuration above but choose a fresh lesson-10 prefix and output file:

```sh
export OPENDATA_PREFIX=opendata-lab/your-unique-trial/express-refresh
BENCH_PRODUCERS=1 BENCH_EVENTS=360 BENCH_BATCH=1 BENCH_RATE=1 \
BENCH_TIMEOUT_S=540 BENCH_OUTPUT=results/express-refresh.csv \
cargo run --locked --offline --example lesson_10
```

This keeps the same storage handles active across an Express session
renewal interval. Verify the full expected record count and inspect stderr for
credential errors. There is no refresh guarantee until an actual AWS trial
passes; retain the CSV and experiment metadata. See lesson 10 for precise
rate, timing, and sample semantics.

AWS references:

- [Directory-bucket authentication and authorization](https://docs.aws.amazon.com/AmazonS3/latest/userguide/s3-express-authenticating-authorizing.html)
- [Express performance design patterns](https://docs.aws.amazon.com/AmazonS3/latest/userguide/s3-express-optimizing-performance-design-patterns.html)

Continue with [failures and writer takeover](LESSON_9.md), then
[controlled measurements](LESSON_10.md). Those experiments must be run on
the actual cloud backend before drawing conclusions about cloud recovery
or milliseconds of takeover latency.
