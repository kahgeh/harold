# Harold Architecture

Harold connects agent sessions running in tmux with notification and reply channels. It keeps durable facts separate from current derived state: observations enter one ordered event stream, a projector derives application state, and consumers see only committed snapshots.

## Why the monitor is event-driven

Agent state has several imperfect sources. A lifecycle hook knows that an agent started or finished work, but a hook can be missed. The visible terminal can repair a missed transition, but the screen can be stale or inconclusive. Process inventory establishes whether an agent is present, but CPU use or elapsed silence does not establish whether it is busy.

Harold records observations instead of letting adapters overwrite a shared row. One serialized monitor runtime resolves the live process identity and appends agent events. A pure reducer then applies the precedence rules, and one application projector owns the current-state database. This keeps acquisition failures from erasing known state and makes restart recovery deterministic.

## Boundaries

```text
Agent hooks ───────────────┐
                          │
tmux and process inventory ├──> serialized monitor ───> durable event stream
                          │                                  │
visible-screen adapter ───┘                                  v
                                                   application projector
                                                            │
                                             ┌──────────────┴──────────────┐
                                             v                             v
                                    current-state database       delivery outbox
                                             │                             │
                                             v                             v
                                    snapshot publisher          notifications/replies
                                             │
                                             v
                                    WatchAgentStates gRPC
```

The boundaries have distinct responsibilities:

| Boundary | Responsibility |
| --- | --- |
| Inventory | Establish live pane and full agent-process incarnation identity. |
| Lifecycle and completion ingress | Submit explicit busy/idle and work-summary observations. |
| Screen adapter | Inspect only the current visible grid and return independent optional state and fallback-summary facts. |
| Monitor runtime | Serialize decisions, deduplicate observations, revalidate departures, and append agent facts. |
| Reducer | Reconcile hook grace, screen repair, incarnation replacement, and summary precedence. |
| Application projector | Atomically update current state, stage externally deliverable work, and advance the checkpoint. |
| Snapshot publisher | Publish only database-backed state after commit. |

Raw captured screen text exists only inside the screen adapter. It is not an event field, projection column, API field, diagnostic value, or application log field. This is a data boundary, not merely a display convention.

## Identity before state

A tmux pane ID alone is not enough to identify an agent over time. A shell can outlive several agent processes, and a PID can be reused. Harold therefore scopes every pane observation to this complete incarnation:

```text
(pane_id, pane_pid, agent_pid, agent_started_at_ms, provider_id)
```

Replacing or restarting an agent creates a new incarnation. The new process begins with `Unknown` state and no work summary; it cannot inherit lifecycle or screen evidence from the previous process. Delayed events remain in durable history but do not mutate a different current incarnation.

## Reconciling lifecycle and screen evidence

Lifecycle evidence is authoritative for the configured grace period, which defaults to two seconds. This allows the terminal to repaint after a hook fires. After grace, a later conclusive screen observation can repair missed or stale lifecycle evidence. Inconclusive screen state preserves the current state.

State and summary are independent. One capture may provide either, both, or neither. Harold retains explicit and screen candidates with their durable observation times; the latest substantive candidate is effective, with explicit winning a tie. This lets a current Busy prompt replace a retained prior completion, while Idle placeholder/absence preserves the current summary. Clearing the explicit candidate can reveal an existing screen candidate.

Current acquisition and ingress reject exact normalized configured idle placeholders before serializing a summary candidate, but older durable events may already contain them. Harold repairs that historical state with an incarnation-scoped, projection-only event that independently clears affected explicit or screen candidates and their timestamps. Because the correction is a durable fact rather than a direct database edit, rebuilding the projection cannot resurrect the placeholder.

Provider screen markers are intentionally configurable because terminal UIs change. When marker matching becomes inconclusive, Harold reports `Unknown` only for an incarnation with no conclusive evidence; it does not infer state from CPU use, tmux activity, or silence. The [agent-monitor reference](../references/agent-monitor/README.md) defines the exact reconciliation and configuration contracts.

## Projection and delivery are separate effects

All durable facts are read in event-stream order. In one state-database transaction, Harold applies agent facts, stages only externally deliverable events, and advances the application checkpoint. Agent observation and health events do not enter the delivery outbox. `TurnCompleted` and `InboundMessageReceived` retain their existing external effects.

Snapshot publication happens after the transaction commits. A late or reconnecting watcher receives the full stored snapshot first, so correctness does not depend on retaining every in-memory notification. The public stream exposes effective state and an optional effective work summary, not the evidence source used to derive them.

Search is also outside this boundary. A dashboard filters the snapshot it already holds; Harold has no search RPC, search field, or persisted query.

## Startup, restart, and shutdown

On startup, Harold opens the durable event stream and checksum-tracked application-state database and projects every historical page to the stream head without publishing. It inspects that complete projection, appends every required legacy-candidate repair in one event-stream batch, projects the repair batch, and reloads the clean snapshot. Only then does it create the publisher and seed the monitor runtime before accepting gRPC traffic. This prevents the first snapshot, an early shutdown, or a 500-event projection-page boundary from exposing a configured placeholder.

During normal operation, the monitor, projector/delivery handler, inbound listener, and gRPC server share a shutdown signal. `SIGINT` or `SIGTERM` closes that signal, stops new monitoring work, closes open `WatchAgentStates` streams, lets the server drain in-flight RPCs, and joins the handler and listener. If the monitor does not stop within one second, Harold aborts that task. Already committed events and projection state remain available at the next start.

For exact events, fields, RPC statuses, failure behavior, and provider limitations, see the [agent-monitor reference](../references/agent-monitor/README.md). To register hooks, follow [Set up agent monitor hooks](../how-tos/setup-agent-monitor-hooks.md).
