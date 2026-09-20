# Lesson 4: reopen the database and resume the monitor

Run twice from this crate's directory:

```sh
cargo run --locked --offline --example lesson_4
cargo run --locked --offline --example lesson_4
```

Each run adds one report from A and one from B. It waits for `flush()`,
closes the writer, opens it again, and reads reports from the saved positions.
After saving the new positions, it opens the database a third time and
asserts that neither stream has unread reports.

The default files are:

| Data | Location relative to this crate | Owner |
| --- | --- | --- |
| Log objects, WAL, manifests | `.data/lesson-4/` | OpenData / SlateDB |
| Per-agent next-read positions | `.checkpoints/lesson-4.json` | Our monitor |

A fresh first run recovers records 0 and 1. Subsequent runs can jump by
thousands of positions: the allocator reserves sequence blocks, so reopening
may leave unused positions. That is not evidence of missing reports. The
example asserts that the exact newly acknowledged positions are recovered.

Follow [the short entry point](examples/lesson_4.rs), then
`durable_round_trip` in [the shared helpers](src/lib.rs). The essential order is:

1. `try_append` accepts the batch; this alone is not a durable acknowledgment.
2. `flush().await` waits for the storage pipeline's durability boundary.
3. Reopen and handle the records.
4. Save the next-read positions only after handling succeeds.

`Checkpoints::save` writes a temporary sibling file, syncs it, renames it,
and syncs its directory. A reader therefore sees an old or new complete
JSON checkpoint, rather than partially overwritten JSON. Corrupt JSON
fails explicitly. The checkpoint records its source configuration and
rejects reuse with a different database path. **Only one monitor may write
a given checkpoint file.** This is not a multi-process checkpoint service.

There are still two separate commits: the handled side effect and the
checkpoint. A crash after printing or sending a notification but before
saving can repeat that work on restart. Use stable event IDs and idempotent
handling for real side effects; lesson 7 demonstrates that boundary.
A local filesystem round trip also does not prove power-loss behavior of
the disk or operating system.

To keep an experiment outside this crate:

```sh
OPENDATA_LOCAL_DIR=/tmp/my-opendata-lab \
OPENDATA_PREFIX=lesson-4 \
OPENDATA_CHECKPOINT=/tmp/my-opendata-monitor.json \
cargo run --locked --offline --example lesson_4
```

Deleting the checkpoint replays stored history. Deleting the database while
keeping its checkpoint is unsafe: the old position may skip new history.
For a clean reset, remove **both** your chosen lesson directory and its
checkpoint, with all processes stopped. Never remove another experiment's
prefix. There is no automatic cleanup of retained log data.

[Lesson 5](LESSON_5.md) repeats the same experiment on S3.
