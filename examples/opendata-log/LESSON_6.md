# Lesson 6: concurrent agents, one writer, an independent monitor

For a local run, clear cloud overrides from lesson 5:

```sh
unset OPENDATA_PREFIX OPENDATA_CHECKPOINT
OPENDATA_BACKEND=local cargo run --locked --offline --example lesson_6
```

Two simulated agents each produce `Started`, `Progress`, and `Finished`.
They send records into a bounded Tokio channel. One writer task receives
those records, appends them to one database, and waits for `flush()` after
each append. Meanwhile a separately opened `LogDbReader` polls each agent's
stream and remembers a separate next-read position.

```text
agent-a ─┐
         ├─ bounded channel (2 records) ─ writer ─ persistent log
agent-b ─┘                                           │
                                        independent polling reader
```

The channel's `send().await` waits when full. A slow storage backend slows
the producers instead of silently dropping their reports. The channel does
not make queued data durable: a process crash can still lose records that
have not reached the acknowledged storage boundary. The agent tasks here
are local simulations, not independent durable remote clients.

Read [the example](examples/lesson_6.rs) in that order: producers, writer,
monitor. Monitor scans happen every 50 ms; the reader refresh interval is
100 ms. These settings add visibility delay even when object storage is
fast. Timing console lines from different tasks is not a reliable latency
measurement.

The monitor has a 30-second catch-up timeout. Its final assertion requires
all six reports exactly once and the three expected messages in order
within each agent's stream. The order between agents may vary. The writer
prints the actual shared sequence numbers; per-agent positions will have
gaps. Independent opens of the in-memory backend do not share one store,
so this example requires local or cloud persistence.

Each invocation uses unique stream keys so a rerun does not mix the new
six reports with older runs. By default the stored log lives under
`.data/lesson-6/`. Checkpoints in this lesson are intentionally in memory;
combine lesson 4's save/load pattern when the monitor itself must restart.
The run keys are printed and history is retained until you clean the
experiment's storage directory/prefix.

To run on S3, supply the lesson 5 AWS environment with a new lesson-6
prefix. To use Express, apply lesson 8's directory-bucket configuration.
The same concurrent producer code runs on either backend. Cloud requests
are real and chargeable; the example does not run them by default.

Next: [structured execution traces](LESSON_7.md).
