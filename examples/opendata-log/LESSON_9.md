# Lesson 9: break a writer, then inspect what survived

Suppose agent A reports “inspected 2 files.” Its writer accepts the event, then
crashes. Was that report saved? Now repeat after the writer confirms `flush()`.
These are different promises. This lesson kills real child processes to make
that difference observable.

From this crate's directory:

```sh
cargo build --locked --offline --example lesson_9
python3 scripts/reliability.py
```

The Rust example is a small JSON-lines server. The Python controller creates a
fresh temporary local object-store directory, starts only its own writers and
readers, and applies six failures. It writes a JSONL evidence file and child
stderr files under `.data/reliability/<unique-run>/`. The controller flushes and
syncs each evidence entry, including the durable acknowledgement ledger. That
ledger lives outside the killed writer and outside the temporary database.
Recovered data is compared with the ledger's sequence, identity, and payload.

| Experiment | What must hold |
| --- | --- |
| Kill after buffered acceptance | The record may survive or disappear; either outcome is allowed. Any recovered payload must be unchanged. |
| Kill after five durable acknowledgements | An independent reader recovers all five, with the acknowledged sequences and payloads. |
| Ignore a successful reply, crash, retry the same source ID | Both appends can exist. A consumer deduplicating the stable `run_id:event_id` presents one event. |
| Disconnect a reader, append eight events, reconnect | The saved next sequence catches up without omissions. A reader left open also eventually observes a later append. |
| Pause A, open writer B, resume A | B durably writes. A's append-and-flush fails with “detected newer DB client”; its stale event is absent on a fresh read. |
| Temporarily replace the owned local object-store directory with a file | A durable append reports a storage error or an application deadline. After restoration and writer restart, earlier acknowledged data survives and new writes succeed. |

The uncertain-reply case deliberately discards the first success **at the
application boundary**; it does not emulate TCP packet loss. Raw evidence keeps
that reply so the experiment can prove it exercised the duplicate case. The
dedup example also rejects the same identity carrying a different payload.
Its dedup map is in memory; persistent production consumer dedup and exactly-once
side effects are separate application work.

The local storage-error injection is safe because the harness alone creates and
owns the temporary directory. A `finally` block restores it, and every child is
resumed if necessary and terminated during cleanup. The experiment demonstrates
error propagation and recovery after restart; it does not promise that a writer
continues after a storage failure. No bucket permissions or cloud objects are
changed to simulate errors.

In repeated local experiments, the interrupted append sometimes returned an
engine error and sometimes kept waiting. This case therefore adds an explicit
one-second application deadline around append/flush. Expiry means **outcome
unknown**, not “the event was never stored.” The writer is killed before storage
is restored; recovery permits that unacknowledged event to be present or absent,
and checks its exact payload if present.

## What takeover actually means

Opening B with the same database path makes B the new writer. This is an active
ownership change, not merely preparing a standby. A can still accept a buffered
append briefly, so checking `try_append()` alone is insufficient. We ask A to
append **and flush**, require the fencing error, then read through an independent
reader to verify that the stale event was never persisted.

The controller records two timings:

* `writer_open_us`: measured inside B around `LogDb::open`.
* `injection_to_durable_us`: measured by the controller from pausing A until B's
  first durable reply has been recorded. This includes process startup, opening,
  append/flush, IPC, and controller evidence writes.

Both clocks are monotonic on one machine. There is no failure detector, election,
lease manager, request router, or production traffic here. A small measured time
therefore proves only that this particular controlled takeover was fast. It
cannot establish a millisecond failover guarantee. The old writer is paused
rather than killed specifically so it can return and challenge the new owner.

Reopened writers can jump to a reserved sequence block (for example, 4096). The
suite checks increasing positions and exact identities, not contiguous numbers.

## Repeat on AWS

First complete the credential/bucket setup and basic reopen test in
[lesson 5](LESSON_5.md) or [lesson 8](LESSON_8.md). With those environment
variables still set:

```sh
python3 scripts/reliability.py --cloud --timeout 120
```

`--cloud` requires `OPENDATA_BACKEND=s3` or `express` and an explicit
`OPENDATA_PREFIX`. Each run appends a random component and a scenario name to
that prefix. Objects are retained for inspection; the script does not delete
cloud data. Request/storage charges apply to the explicitly selected bucket.
The local-directory storage-error experiment is recorded as skipped on AWS.

Without `--cloud`, this script always uses a new local directory, even if the
shell contains cloud settings. A cloud run is a separate acceptance result; a
local pass does not establish S3 durability, session refresh, AZ resilience, or
real network failover. The timeout bounds individual replies; it is a failure
threshold, not a performance target.

## Read the code in this order

1. `Child.append()` persists the durable-ack ledger only after a successful reply.
2. `crash_durable()` kills the writer and checks an independent reader against it.
3. `fenced_takeover()` pauses A, opens B, resumes A, and checks the error and data.
4. `examples/lesson_9.rs` translates the JSON protocol into actual OpenData calls.

The local suite was run successfully during implementation. Its generated
evidence records the observations from that run. Cloud execution requires your
explicit bucket and credentials and is not implied by that result. Next,
[lesson 10](LESSON_10.md) measures latency and throughput under bounded load.
