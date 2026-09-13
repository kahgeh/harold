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
provider screen adapters ─┘                                  v
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
| Pane capture | Capture the visible grid for state or a bounded styled history tail for task recovery. |
| Provider screen adapter | Interpret captured state markers and identify submitted prompt blocks using the configured rendering rules. |
| Monitor runtime | Establish which submitted blocks belong to the current incarnation, serialize decisions, deduplicate observations, revalidate departures, and append agent facts. |
| Activity summarizer | Generate a bounded description from supplied task/outcome evidence without blocking the monitor. |
| Reducer | Reconcile hook grace, screen repair, incarnation replacement, and summary precedence. |
| Application projector | Atomically update current state, stage externally deliverable work, and advance the checkpoint. |
| Snapshot publisher | Publish only database-backed state after commit. |

Raw captured screen text exists only inside the capture and screen-adapter boundary. The runtime receives fingerprints and bounded candidate instructions. Raw captures are not event fields, projection columns, API fields, diagnostic values, or application log fields.

## Identity before state

A tmux pane ID alone is not enough to identify an agent over time. A shell can outlive several agent processes, and a PID can be reused. Harold therefore scopes every pane observation to this complete incarnation:

```text
(pane_id, pane_pid, agent_pid, agent_started_at_ms, provider_id)
```

Replacing or restarting an agent creates a new incarnation. The new process begins with `Unknown` state and no work summary; it cannot inherit lifecycle or screen evidence from the previous process. Delayed events remain in durable history but do not mutate a different current incarnation.

## Reconciling lifecycle and screen evidence

Lifecycle evidence is authoritative for the configured grace period, which defaults to two seconds. This allows the terminal to repaint after a hook fires. After grace, a later conclusive screen observation can repair missed or stale lifecycle evidence. Inconclusive screen state preserves the current state.

State and summary are independent. A monitoring pass may provide either, both, or neither through its visible-state capture and any eligible history recovery. Harold retains explicit and screen candidates with their durable observation times; the latest substantive candidate supplies the source fallback, with explicit winning a tie. A generated description matching the current activity revision takes precedence when one exists. This lets a current Busy prompt replace a retained prior completion, while Idle placeholder/absence preserves the current summary. Clearing the explicit candidate can reveal an existing screen candidate.

Current acquisition and ingress reject exact normalized configured idle placeholders before serializing a summary candidate, but older durable events may already contain them. Harold repairs that historical state with an incarnation-scoped, projection-only event that independently clears affected explicit or screen candidates and their timestamps. Because the correction is a durable fact rather than a direct database edit, rebuilding the projection cannot resurrect the placeholder.

Provider screen markers are intentionally configurable because terminal UIs change. When marker matching becomes inconclusive, Harold reports `Unknown` only for an incarnation with no conclusive evidence; it does not infer state from CPU use, tmux activity, or silence. The [agent-monitor reference](../references/agent-monitor/README.md) defines the exact reconciliation and configuration contracts.

## Recovering a submitted task from scrollback

If a Codex hook misses a task and subsequent output pushes its prompt offscreen, Harold can recover that instruction from a bounded tmux history tail. State classification still reads the visible grid. History is captured separately when a process is first observed, when work starts, and at limited recovery retries.

The selected adapter determines which terminal rows count as submitted input. `codex-v1` uses the styled prompt boundary to distinguish a submitted instruction from an unsent composer draft, even when the draft text has normal styling. `generic-v1` uses configured safe prefixes. Claude currently uses the generic adapter; OpenCode supplies screen state only and relies on its plugin for submitted tasks.

Retained text alone cannot establish which process submitted it. Harold therefore records an initial sequence of prompt fingerprints for each incarnation and adopts none of those existing prompts. Later captures must overlap that sequence before newly added submissions can become task evidence. If the tail loses every prior anchor, Harold establishes another baseline without adopting its contents. This deliberately means a late attachment can miss the task already underway. A new post-baseline submission can be recovered, while a replacement process cannot inherit old pane history.

Only fingerprints remain in the acquisition checkpoint, and that checkpoint is discarded on departure or restart. Durable source candidates continue to survive a Harold restart. A newly recovered occurrence advances the activity revision even when the user submits identical text again; repeated captures of the same occurrence do not. See [provider screen adapters](../references/agent-monitor/screen-adapters.md) for configuration, capture timing, and recovery limits.

## Readable activity descriptions

When activity summarization is enabled, Harold retains the submitted instruction as fallback and updates Busy/Idle as usual. A background Claude request then turns the available evidence into a short description. Completion hooks include the agent's reply, allowing the description to state a reported outcome such as a fix with tests still running. An instruction alone provides only the task, so it must not be treated as proof of completion.

The description is a separate candidate tied to the source activity's event-stream revision and full process incarnation. Both runtime and reducer check that identity before accepting a result. This keeps a slow response from overwriting a newer task, and keeps old pane history from crossing a process replacement. Original instructions remain available when generation fails.

Only an accepted generated result becomes a durable event. Its projection survives restart and replay, while pending requests remain ephemeral. Generation is bounded and independent of the notification outbox, so describing work cannot send a notification or recursively request another description. See the [activity-summary reference](../references/agent-monitor/activity-summaries.md) for configuration and evidence limits.

## Projection and delivery are separate effects

All durable facts are read in event-stream order. In one state-database transaction, Harold applies agent facts, stages only externally deliverable events, and advances the application checkpoint. Agent observation and health events do not enter the delivery outbox. `TurnCompleted` and `InboundMessageReceived` retain their existing external effects.

Snapshot publication happens after the transaction commits. A late or reconnecting watcher receives the full stored snapshot first, so correctness does not depend on retaining every in-memory notification. The public stream exposes effective state and an optional effective work summary, not the evidence source used to derive them.

Search is also outside this boundary. A dashboard filters the snapshot it already holds; Harold has no search RPC, search field, or persisted query.

## Startup, restart, and shutdown

On startup, Harold opens the durable event stream and checksum-tracked application-state database and projects every historical page to the stream head without publishing. It inspects that complete projection, appends every required legacy-candidate repair in one event-stream batch, projects the repair batch, and reloads the clean snapshot. Only then does it create the publisher and seed the monitor runtime before accepting gRPC traffic. This prevents the first snapshot, an early shutdown, or a 500-event projection-page boundary from exposing a configured placeholder.

During normal operation, the monitor, projector/delivery handler, inbound listener, and gRPC server share a shutdown signal. `SIGINT` or `SIGTERM` closes that signal, stops new monitoring work, closes open `WatchAgentStates` streams, lets the server drain in-flight RPCs, and joins the handler and listener. If the monitor does not stop within one second, Harold aborts that task. Already committed events and projection state remain available at the next start.

For exact events, fields, RPC statuses, failure behavior, and provider limitations, see the [agent-monitor reference](../references/agent-monitor/README.md). To register hooks, follow [Set up agent monitor hooks](../how-tos/setup-agent-monitor-hooks.md).
