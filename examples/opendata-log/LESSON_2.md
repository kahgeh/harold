# Lesson 2: two agents, separate streams

Agent A inspects files while Agent B runs checks. They take turns reporting
progress to the same log. A monitor can ask for either agent's history.

## Run it

From Harold's repository root:

```sh
cd examples/opendata-log
cargo run --locked --example lesson_2
```

The complete program is [examples/lesson_2.rs](examples/lesson_2.rs).
Lesson 1 remains available with `cargo run --locked`.

Expected output:

```text
Append order (one shared log):
  [0] agent-a: Started
  [1] agent-b: Started
  [2] agent-a: Progress: inspected 2 files
  [3] agent-b: Progress: ran 3 checks
  [4] agent-a: Finished
  [5] agent-b: Finished

Stream agent-a:
  [0] agent-a: Started
  [2] agent-a: Progress: inspected 2 files
  [4] agent-a: Finished

Stream agent-b:
  [1] agent-b: Started
  [3] agent-b: Progress: ran 3 checks
  [5] agent-b: Finished
```

The first section prints the sequence actually returned by each append. The
next sections print the records actually read from the log.

## Follow what changes

1. Agent A appends `Started` under key `agent-a`. OpenData assigns sequence `0`.
2. Agent B appends `Started` under key `agent-b`. The same counter advances to `1`.
3. Agent A appends its next report and receives `2`, because `1` already belongs
   to Agent B's report.
4. `scan(Bytes::from("agent-a"), ..)` returns only A's records, in sequence order.
5. Scanning `agent-b` returns only B's records. Reading A did not consume or
   remove B's reports, or A's reports.

**The key chooses whose history to read. The sequence identifies the record's
position in the shared log.** Each stream keeps the original positions; scans
do not renumber them from zero.

That is why `0, 2, 4` is a complete history for Agent A in this run. Its gaps
are positions used by Agent B. A gap by itself is not evidence of event loss;
later reliability tests will check stable source event IDs for completeness.

## Connect this to lesson 1

In lesson 1, the monitor remembered one past the last sequence it read.
The same rule works when streams have gaps: after reading A's sequence `0`,
scanning A from `1..` finds its next records at `2` and `4`.
The start position is a lower bound; it does not need to be a record for A.

Also notice that both streams contain **three** records, but B's last sequence
is `5`. A count of records is not a checkpoint. Lesson 3 will give each stream
its own saved next-read position.

## Try one change

Change Agent B's first report in the `reports` array from
`("agent-b", "Started")` to `("agent-a", "Started")` and run again.

The report still gets sequence `1`, but it now appears in A's history. A has
four records and B has two. OpenData chooses the stream from the key you
provide; it does not infer which agent produced the text. Undo the change
before continuing.

## What this lesson establishes

There is one `LogDb` and two keys. The two agents are simulated by a fixed list
of reports, appended sequentially. They are not separate processes or competing
database writers. Later lessons introduce concurrent producers.

Storage is still explicitly `InMemory`. Flushing waits for the pipeline but
does not create a database file; every run starts empty.

Continue with [lesson 3: separate monitor positions](LESSON_3.md), or return to
the [lesson roadmap](README.md#where-we-go-next) or the
[lesson 1 walkthrough](README.md#run-lesson-1).
