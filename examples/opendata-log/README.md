# Follow an agent's progress

An agent starts a task, reports that it inspected two files, then finishes.
A monitor reads the first two reports. When it checks again, it reads only
the new `Finished` report.

This is lesson 1 of a small progression toward monitoring several agents
and testing the reliability and performance of their event streams.
The agent is simulated by ordinary Rust code. No model, API key, or running
Harold service is needed.

Ready for two agents? Continue with [Lesson 2: separate streams](LESSON_2.md),
or run `cargo run --locked --example lesson_2` from this crate's directory.

## Run lesson 1

From this repository's root:

```sh
cd examples/opendata-log
cargo run --locked
```

Use a current stable Rust toolchain. The first run compiles OpenData and its
storage dependencies, so it takes longer than this small program suggests.
This directory is its own Cargo workspace, with its own lockfile.

Expected output:

```text
First monitor read:
  [0] agent-a: Started
  [1] agent-a: Progress: inspected 2 files
Monitor checkpoint: next sequence = 2

Second monitor read (after another report):
  [2] agent-a: Finished
Monitor checkpoint: next sequence = 3

Third monitor read (nothing new):
  No new reports.
```

Each run starts empty and produces the same output. Both the reports and
the monitor's checkpoint live in memory for this lesson.

## Read the code from top to bottom

Open [src/main.rs](src/main.rs).

1. **Give the agent a stream.** Every report uses `agent-a` as its `key`.
   OpenData groups records with the same key into one ordered stream.
2. **Append two reports.** Each `Record` contains a key and a value, both
   bytes. OpenData stores the progress text without interpreting it and assigns
   the records sequence numbers `0` and `1`.
3. **Let the monitor read.** `scan(agent, 0..)` reads that agent's records
   starting at sequence `0`, including `0` itself.
4. **Remember where to resume.** After reading sequence `1`, the monitor
   remembers `2`. That variable is its checkpoint: the next position to read.
5. **Append and catch up.** `Finished` receives sequence `2`.
   Reading from `2..` returns just that report. Reading from `3..` returns none.

Reading does not remove records. The earlier reports remain in the log;
the monitor skips them because it supplies a later start position.

`try_append` returning successfully does not mean a write has become durable.
The example calls `flush` before reading to wait for the write pipeline.
Here the storage is explicitly `InMemory`: even after a flush, everything
disappears when the process exits. Disk persistence comes in a later lesson.

The three reads happen sequentially. There is no background monitor yet.

## Try a small change

Change the **second** `read_progress` call to pass `0` instead of
`next_sequence`. Run it again: the second read now prints all three reports.
You changed what the monitor remembers, not what the log stores.

## Where we go next

All ten lessons have runnable examples. Follow them in order; cloud lessons
require your own existing bucket and credentials. Local checks and unrun cloud
checks are recorded in [verification results](VERIFICATION.md).

| Lesson | Add | Observe |
| --- | --- | --- |
| [2](LESSON_2.md) | A second agent with key `agent-b` | Each scan selects one agent; shared sequence numbers can have gaps within a stream. |
| [3](LESSON_3.md) | A monitor for both agents | Keep a separate next-read position for each stream. |
| [4](LESSON_4.md) | A local storage directory and saved checkpoints | Close, reopen, and resume; storing reports and storing checkpoints are separate responsibilities. |
| [5](LESSON_5.md) | S3 object storage for the log | Write reports to a bucket, wait for durability, then reopen the same log after a restart. Keep the monitor's saved checkpoints local for this lesson. |
| [6](LESSON_6.md) | Concurrent simulated agents and a polling monitor | Agents append while the monitor catches up. |
| [7](LESSON_7.md) | Structured agent execution traces | Preserve run/event identity and causal links; reconnect and recover events from durable history. |
| [8](LESSON_8.md) | S3 Express One Zone | Repeat storage/replay checks with a directory bucket, then compare placement and durability tradeoffs with S3 Standard. |
| [9](LESSON_9.md) | Failures and writer takeover | Crash or pause writers, retry uncertain writes, and verify recovery, fencing, and reader catch-up. |
| [10](LESSON_10.md) | Controlled performance experiments | Measure acceptance, durability, reader visibility, throughput, tail latency, and recovery separately. |

Lesson 5 builds on lesson 4 by changing where the log is stored. It covers the bucket, region, storage prefix, and AWS credentials, then repeats the
append, close, reopen, and replay exercise. The monitor still owns its
checkpoints; putting the log in S3 does not automatically save them there.

Sequence numbers belong to the whole log, so a stream's next record need not
be exactly one greater than its previous record. Using `last_seen + 1` as
the inclusive scan start still works: the scan returns the next matching
record at or after that position.

The [experiment plan](EXPERIMENTS.md) defines the workload, failure cases,
measurement boundaries, and evidence to collect. Run [lesson 9](LESSON_9.md) for recovery checks and [lesson 10](LESSON_10.md)
for raw latency measurements. AWS acceptance remains separate from local results.

## Keep the example focused

Event streams, agent execution traces, local/S3 storage, reliability, and
performance experiments stay in this crate. The broader multi-user runtime
and workflow capture/refinement/reuse vision lives in the independent
[Iterant repository](../../../iterant/README.md).

ActiveGraph is the trace-model reference. Lesson 7 streams simulated
execution events; an integration with the actual Python runtime is a separate
step. Workflow execution remains in Iterant.

## Versions and reference

This example pins published `opendata-log` 1.0.0 and `opendata-common` 0.1.17.
Those releases use the same SlateDB generation. Upstream `main` may show a
different API or dependency combination.

- [Published LogDb API](https://docs.rs/opendata-log/1.0.0/log/struct.LogDb.html)
- [Published LogRead API](https://docs.rs/opendata-log/1.0.0/log/trait.LogRead.html)

## Configure cloud storage later

You can complete the local lessons before choosing any AWS resources. The
examples read cloud settings at runtime; no region or bucket is hardcoded.
Sydney in the walkthroughs is an example, not a required default.

| Setting | Environment variable | When needed |
| --- | --- | --- |
| Storage backend | `OPENDATA_BACKEND` | Defaults to `local`; choose `s3` or `express` for cloud runs. |
| AWS region | `AWS_REGION` | Required for either cloud backend. |
| Bucket name | `AWS_BUCKET` | Required for either cloud backend; set to the bucket for that run. |
| Database prefix | `OPENDATA_PREFIX` | Required for cloud runs; use an isolated experiment prefix. |
| Express mode | `AWS_S3_EXPRESS` | Set to `true` for Express; unset or `false` for Standard. |
| Local checkpoint file | `OPENDATA_CHECKPOINT` | Optional; defaults to a lesson-specific file under `.checkpoints/`. |

Leave cloud settings unset for now. Supply them when you are ready to run
[lesson 5](LESSON_5.md) or [lesson 8](LESSON_8.md). Credentials can also wait;
see lesson 5 for the supported credential setup. Setting an AWS CLI profile
alone does not guarantee this Rust client's authentication is configured.
Live cloud verification remains pending until those details are available.

## Run the whole local progression

```sh
python3 scripts/verify_local.py
```

Run lesson 1 once first to populate the dependency cache. The verifier uses
Cargo offline, then builds and runs lessons 1–4, 6–7, the failure suite (9), and a small benchmark
(10) against isolated local data. It checks that lessons 5 and 8 reject missing
cloud configuration. It does **not** turn those rejection checks into AWS results.
See [lesson 5](LESSON_5.md) and [lesson 8](LESSON_8.md) to run real cloud trials.

[Dependency audit scope](DEPENDENCIES.md) documents the pinned XML parser
advisories and the restrictions on these cloud experiments.
