# Lesson 7: reconnect to execution traces

Two simulated agents each start, request a tool, receive its result and finish.
The monitor disconnects after the requests. The agents keep writing. When the
monitor reconnects, it recovers the results and finishes from persisted history.
One report is deliberately retried with the same source identity.

```sh
cargo run --locked --example lesson_7
```

The default store is `.data/lesson-7`. Each invocation uses a new run ID. For an
isolated directory, set `OPENDATA_LOCAL_DIR` as in [lesson 4](LESSON_4.md).
This example requires local or cloud storage: an independent in-memory reader
would have a separate empty store.

Read [examples/lesson_7.rs](examples/lesson_7.rs). `TraceEvent` carries the run ID,
event ID, agent, actor, frame, event type, payload and causal parent. These are
application fields serialized as JSON; OpenData only sees a stream key and bytes.
For example, `agent-a-2` is a tool result caused by `agent-a-1`, the tool request.
The `(run_id, event_id)` pair identifies a source event. The storage sequence tells
the monitor where it read that event; retrying an append allocates another
sequence even when its source identity is unchanged.

The monitor remembers two different things:

- A next-read position for each stream, advanced after successful processing.
- Events already applied to its projection, keyed by source identity.

The retried tool result appears in storage twice and is applied once. A repeated
identity with a different payload is an error. The program then discards the
monitor state and rebuilds it from sequence zero, checking that both projections
agree: **9 stored records, 8 unique events, two complete causal chains**.

The monitor's positions and projection are in memory here. A production consumer
would persist the deduplication state, projection and cursor together, or rebuild
all three from retained history. Saving only a cursor can skip effects after a
crash. This example validates causal chains within an agent's stream; cross-agent
causal dependencies can arrive out of scan order and would need deferred joining.

These are simulated execution events inspired by ActiveGraph's event fields.
There is no live Python integration or token feed. ActiveGraph event IDs must be
qualified by run ID; a later adapter must recover from its durable event store
because its bounded live sink can drop events. Workflow capture and refinement
belong to Iterant.

Try changing the retry's payload while keeping its ID: the monitor rejects the
conflicting history. Then continue with [lesson 8](LESSON_8.md).
