# Verification of the ten-lesson progression

All ten lessons have runnable code and walkthroughs. **Real S3 Standard and S3
Express acceptance remains pending:** no target buckets were supplied for this
run. Nothing was provisioned in AWS. Missing-cloud-configuration checks verify
input handling only.

## Local evidence — September 20, 2026

Environment: Darwin arm64, Rust 1.97.1, debug builds. The standalone workspace
remains separate from Harold. The parent repository revision was
`2249b0290520063fa5dc01f48bee914efc0a2001`, with these examples uncommitted.
Cargo.lock SHA-256:
`aab7c0e83ec3b293936e7d31e64d699e31c081ea1638520c66086dda2b65be15`.

`python3 scripts/verify_local.py` passed 17 checks. The runner retains logs,
source hashes, lock identity, raw benchmark samples, and fault evidence under
`results/local-<time>/`. Initial full-run evidence is
`results/local-1789891031365548000/` (local generated files, deliberately ignored
by Git). Its checks included:

- Rust format, all-target tests, strict Clippy, and all executable builds.
- Lessons 1–4, 6–7, including a second lesson-4 run using saved checkpoints.
- Lessons 5/8 rejecting the wrong backend, and the cloud endpoint override guard.
- All six actual child-process failure scenarios in lesson 9.
- Lesson 10's 40-event workload and invalid workload rejection.

The storage integration test checks reopened history, per-stream positions,
sequence gaps after restart, and failure on corrupt checkpoint JSON. Example
assertions and the Python failure runner provide the remaining end-to-end
checks; `cargo test` alone does not execute example `main` functions.

The final full rerun, after adding queue sampling, configurable reader refresh
and process resource collection, also passed all 17 checks. Its evidence is
`results/local-1789891295157296000/`. The wrapper recorded a successful 40-event
child, 0.048556 CPU user seconds, 0.092465 CPU system seconds and 26,836,992 bytes
peak RSS for its entire lifetime. These are local debug smoke measurements,
not resource capacity estimates. The source hashes in that run cover the final
code. Higher offered-rate checks separately exercised nonzero queue backlog.

## What the failure experiments observed

All five durably acknowledged records survived the killed writer and were read
by an independent reader. The accepted-but-unconfirmed record did not survive
in the initial full run; either survival outcome is allowed by the assertion.
The uncertain-reply retry produced two stored records and one deduplicated event.
The disconnected reader recovered the backlog and then observed a live append.

After pausing A and opening B, A's append-and-flush returned
`Closed error: detected newer DB client`. A fresh reader found B's acknowledged
records and no stale A record. In this single local run, B's writer open was
4.626 ms and the controller's pause-to-durable-response interval was 9.881 ms.
These include the measured local configuration only: no election, network
router, failure-detection delay, AWS, AZ failure, or machine power loss was tested.
**These observations do not establish a millisecond failover guarantee.**

Temporarily replacing the owned local storage directory surfaced a storage
error in the initial full run. Other implementation trials reached the explicit
one-second application deadline instead. Both paths preserve an unknown outcome
for the interrupted write and reconcile previously acknowledged history after
storage restoration.

## How to interpret the performance smoke test

The initial 40-event debug run measured p50 append acceptance of 0.171 ms,
scheduled-arrival-to-durable acknowledgment of 3.502 ms, and
scheduled-arrival-to-independent-reader observation of 72.699 ms. The reader's
scan interval was 20 ms and storage refresh interval 100 ms. These timings have
different endpoints; reader polling is part of the observation measurement.

All 40 payloads, identities, ordering and sequences matched; there were no
missing observations or operation errors. Forty correlated batch observations
are too few for tail-performance conclusions. Use release builds, repeated
trials and controlled placement as described in [lesson 10](LESSON_10.md).
The reviewer also exercised 100 producers / 1,000 events successfully, and
verified that a workload timeout exits unsuccessfully with retained partial CSV.

The completion reviewer approved the final implementation with no blocking
findings and verified that source hashes match the final local run.

## Remaining external acceptance

Follow [lesson 5](LESSON_5.md) and [lesson 8](LESSON_8.md) with actual bucket,
region, credentials and isolated prefixes. Then run lesson 9 with `--cloud` and
lesson 10 with the same workload/compute placement for both backends. Express
credential renewal has a separate six-minute recipe. The harness skips local
filesystem fault injection on cloud runs; it does not simulate an AWS outage.

The [dependency audit scope](DEPENDENCIES.md) records unresolved XML parser
advisories. Local verification and a future successful AWS trial do not remove
those advisories or establish production readiness.
