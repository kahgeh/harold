# Agent Monitor Reference

The agent monitor discovers configured agent processes in tmux, records lifecycle and provider-screen observations, projects current pane state, and serves complete snapshots over gRPC.

## Problem

A pane name does not answer whether its current agent process is busy, idle, or newly replaced. Operators need concrete answers to questions such as: which process does this row describe, which input wins when a hook and the terminal disagree, what survives a restart, and what does a reconnecting consumer receive? They also need those answers without storing terminal history or exposing raw pane content.

## Architecture

The monitor has one durable write path and one current-state projection path.

```text
┌──────────────────────────────┐
│ Acquisition adapters         │
└──────────────┬───────────────┘
               │ typed observations
               v
┌──────────────────────────────┐
│ AgentMonitorRuntime          │
└──────────────┬───────────────┘
               │ ordered append
               v
┌──────────────────────────────┐
│ EventStream: harold/main     │
└──────────────┬───────────────┘
               │ version order
               v
┌──────────────────────────────┐
│ Application projector        │
└──────────────┬───────────────┘
               │ atomic commit
               v
┌──────────────────────────────┐
│ harold-state.db              │
└──────────────┬───────────────┘
               │ complete snapshot
               v
┌──────────────────────────────┐
│ WatchAgentStates consumers   │
└──────────────────────────────┘
```

Inventory, screen capture, and lifecycle adapters produce typed inputs. `AgentMonitorRuntime` serializes agent-event decisions, deduplicates observations only after successful append, and revalidates departure. The application projector reads durable events in stream-version order and is the only writer of the current projection. The snapshot publisher distributes committed database state; it is not a second state store.

### Incarnation identity

Every pane-scoped agent event identifies the complete current agent incarnation:

| Field | Meaning |
| --- | --- |
| `pane_id` | tmux pane ID, such as `%22` |
| `pane_pid` | Long-lived pane-root process ID |
| `agent_pid` | Selected configured agent process ID |
| `agent_started_at_ms` | OS-reported agent-process start time |
| `provider_id` | Named provider ID, or `unknown` for an ambiguous provider match |

Any change creates a new incarnation. A replacement begins at `Unknown` with no lifecycle state, screen state, explicit summary, or fallback summary. Events for a different incarnation remain in history but are ignored by the current projection.

## Interaction diagrams

### Observation, projection, and publication

```mermaid
sequenceDiagram
    participant Inventory as tmux/process inventory
    participant Hook as Lifecycle or stop hook
    participant Screen as Provider screen adapter
    participant Runtime as AgentMonitorRuntime
    participant Events as EventStream harold/main
    participant Projector as Application projector
    participant StateDB as harold-state.db
    participant Hub as Snapshot publisher
    participant Dashboard as Dashboard consumer

    par scheduled inventory
        Inventory->>Runtime: complete pane and process observations
    and explicit lifecycle
        Hook->>Runtime: pane ID, Busy/Idle, optional summary update
    and screen acquisition
        Screen->>Runtime: matching incarnation, optional state, ordered prompt scan
    end
    Runtime->>Runtime: align incarnation checkpoint and select eligible fallback
    Runtime->>Events: append meaningful ordered facts
    Events->>Projector: events after application checkpoint
    Projector->>StateDB: begin transaction
    Projector->>StateDB: reduce pane/health state and selectively stage outbox work
    Projector->>StateDB: advance checkpoint and commit
    Projector->>StateDB: load complete committed snapshot
    Projector->>Hub: publish greater event-stream revision
    Hub-->>Dashboard: complete AgentStateSnapshot
```

### Startup, reconnect, and shutdown

```mermaid
sequenceDiagram
    participant OS
    participant Harold
    participant Events as EventStream
    participant StateDB as harold-state.db
    participant Hub as Snapshot publisher
    participant Client as Watch client

    Harold->>StateDB: initialize or verify the current schema
    Harold->>Events: load all pages after stored checkpoint
    Harold->>StateDB: atomically project through stream head
    Harold->>StateDB: load complete projected snapshot
    Harold->>Hub: create and seed snapshot publisher
    Harold->>Harold: seed monitor runtime from stored snapshot
    Harold->>OS: bind gRPC listener
    Client->>Harold: WatchAgentStates({})
    Harold-->>Client: current complete snapshot first
    Hub-->>Client: later snapshots with greater through_event_version
    Client-xHarold: disconnect
    Client->>Harold: reconnect WatchAgentStates({})
    Harold-->>Client: latest complete snapshot first
    OS->>Harold: SIGINT or SIGTERM
    Harold-->>Client: close watch stream
```

## Durable event contract

Agent events use the existing ordered `harold/main` `EventStream`. Append batches are atomic at the event-stream boundary.

| Event | Durable fields | Projection effect |
| --- | --- | --- |
| `AgentPaneObserved` | Pane metadata, provider display data, full incarnation, observation time | Inserts or refreshes the matching pane. A new incarnation replaces the row and starts `Unknown` with empty evidence. |
| `AgentPaneDeparted` | Full incarnation, observation time | Removes the row only when the full incarnation still matches. |
| `AgentLifecycleObserved` | Full incarnation, `Busy`/`Idle`, adapter ID, `Unchanged`/`Clear`/`Set` summary update, observation time | Updates matching hook evidence and explicit-summary candidate. |
| `AgentScreenObserved` | Full incarnation, optional `Busy`/`Idle`, optional normalized fallback summary, classifier ID, observation time | Applies each present fact independently. An absent field preserves its candidate. |
| `AgentActivitySummaryGenerated` | Full incarnation, source basis version, generated description, generation time | Sets a generated candidate only for the matching current incarnation and activity revision; does not change agent state or stage delivery. |
| `AgentMonitorHealthChanged` | Component, healthy/degraded flag, bounded reason code, observation time | Upserts health for the component. |
| `TurnCompleted` | Five notification fields plus optional resolved incarnation and `Unchanged`/`Set` completion summary update | Always preserves notification behavior; a matching resolved incarnation also supplies idle evidence and a non-destructive summary update. |

`ReportAgentState` resolves the current incarnation and appends `AgentPaneObserved` immediately before `AgentLifecycleObserved` in one batch. A resolved `TurnComplete` appends `AgentPaneObserved` immediately before `TurnCompleted`. An unresolved completion still appends `TurnCompleted` for notification but does not alter agent state.

Repeated inventory metadata and repeated captures of the same screen evidence do not append events. A screen event is appended when its state changes or the acquisition checkpoint proves a new eligible submitted occurrence. That occurrence can repeat the previous instruction text. State and summary do not both need to be present.

## Reconciliation contract

### State

1. Inventory owns presence. A pane is departed only after two complete successful scans omit it and a fresh lookup confirms the exact incarnation is no longer current.
2. A lifecycle observation supplies `Busy` or `Idle` and clears the prior screen-state epoch.
3. Lifecycle state wins during `agent_monitor.hook_grace_ms` after the observation.
4. At or after grace, a conclusive screen observation can replace the effective lifecycle state.
5. Within one visible grid, a matching busy clause wins over a matching idle clause.
6. Inconclusive screen state preserves existing evidence. With no conclusive lifecycle or screen evidence, effective state is `Unknown`.
7. `last_transition_at_ms` changes only when effective state changes.

The runtime retains a conflicting screen state during grace only as an acquisition result; it does not append that state until a post-grace sample still shows it. Later identical samples are deduplicated within that lifecycle epoch. Summary changes remain independent and can append during hook grace.

### Work summaries

Harold keeps an explicit candidate and a provider-screen candidate for each incarnation, with durable internal observation timestamps. The most recently observed substantive source candidate is selected, with explicit winning an equal-timestamp tie. When enabled, a valid generated activity description takes precedence for the same source revision. A new incarnation starts without any candidate. See [AI activity summaries](activity-summaries.md) for generation, fallback, and configuration.

| Input | Value | Explicit-summary effect |
| --- | --- | --- |
| `ReportAgentState.work_summary` | Absent | Preserve (`Unchanged`) |
| `ReportAgentState.work_summary` | Present, normalizes to empty | Clear explicit candidate; reveal fallback if one exists |
| `ReportAgentState.work_summary` | Present, normalizes non-empty and is not an exact configured idle placeholder | Set explicit candidate |
| `ReportAgentState.work_summary` | Exact normalized configured idle placeholder | Preserve (`Unchanged`) before event serialization |
| `TurnComplete.last_user_prompt` | Normalizes to empty | Preserve (`Unchanged`) because this proto3 scalar does not distinguish absent from empty |
| `TurnComplete.last_user_prompt` | Normalizes non-empty and is not an exact configured idle placeholder | Set explicit candidate when the completion resolves to the current incarnation |
| `TurnComplete.last_user_prompt` | Exact normalized configured idle placeholder | Clear the raw prompt and preserve (`Unchanged`) before event serialization |
| Provider-screen fallback | Inconclusive or no new submitted occurrence | Preserve prior fallback |
| Provider-screen fallback | Newly acquired substantive submitted occurrence | Replace the screen candidate and advance the activity revision, even for repeated text; it becomes the source fallback when newer than the explicit candidate |

All summary inputs pass through the same terminal sanitizer. It removes C0 and C1 controls and complete ESC control sequences, collapses Unicode whitespace to single spaces, trims the result, and truncates it to 160 Unicode scalar values. Screen acquisition and the runtime defense reject only exact equality with a normalized configured idle fragment; a substantive prompt that merely mentions the placeholder remains valid. A conclusive state from the same observation remains usable, and placeholder/absence does not refresh or clear the prior screen candidate.

Lifecycle and completion input use only the matching provider's idle fragments when the incarnation names a currently configured provider. If the provider ID is missing, `unknown`, or no longer configured, Harold compares against every configured provider's fragments. An exact match becomes `Unchanged` before event serialization; completion also clears the raw prompt. Another provider's placeholder remains legitimate for a known provider, and text that merely contains a placeholder remains substantive. For example, `Explain why the UI says Ask Codex to do anything` is a valid instruction.

Placeholder rules apply when observations arrive. Changing the configuration does not retroactively change stored summaries.

The public API exposes one optional effective summary. When it is absent, the dashboard—not Harold—owns the exact display copy `No work summary reported`.

## Projection and storage

A fresh installation creates `<store.path>/harold-state.db` from one complete schema, `001_initial.sql`. Its checksum is recorded in `_migrations` and verified on subsequent opens. The application tables are:

| Table | Contents |
| --- | --- |
| `last_processed_event` | Last event-stream version processed for each namespace and partition |
| `delivery_outbox` | Pending and completed external delivery work, including retry state |
| `agent_panes` | Pane/display metadata, full incarnation, hook and screen evidence, explicit and fallback summary candidates with internal timestamps, effective state/summary, last transition, and last event version |
| `agent_monitor_health` | Component, healthy flag, bounded reason code, observation time, and last event version |

`agent_panes` includes `summary_basis_version`, `generated_work_summary`, and `generated_summary_basis_version` from creation. These keep a generated description tied to its source revision while preserving the original source candidates. Stores created with this schema can reopen and replay normally. Incompatible schemas are rejected; Harold does not convert or reset them.

The state database uses WAL mode, `synchronous = NORMAL`, and a five-second busy timeout. For each projection batch, Harold opens one immediate transaction, applies agent rows, stages only externally deliverable events, advances `last_processed_event`, and commits. An error rolls the whole transaction back.

Only `TurnCompleted`, `InboundMessageReceived`, and unknown event types are staged in the delivery outbox. Agent observation, generated-summary, and monitor-health events are projection-only. Unknown event types remain visible to the existing permanent-delivery failure path instead of being silently skipped.

After commit, Harold loads the checkpoint, health, and panes with one query and publishes the complete snapshot if its `through_event_version` is greater than the in-memory revision. A revision can advance because of a non-agent event while pane content remains unchanged.

## gRPC contract

The canonical schema is `harold-api/proto/harold.proto`. The service exposes exactly these methods:

| Method | Request | Response semantics |
| --- | --- | --- |
| `TurnComplete` | Existing five scalar fields: `pane_id = 1`, `pane_label = 2`, `last_user_prompt = 3`, `assistant_message = 4`, `main_context = 5` | Unary `accepted = true` after durable append. Existing field numbers are unchanged. |
| `ReportAgentState` | `pane_id = 1`, `state = 2`, `adapter_id = 3`, `optional work_summary = 4` | Unary `accepted = true` after the pane-plus-lifecycle batch is durably appended. Projection may follow asynchronously. |
| `WatchAgentStates` | Empty request | Server stream whose first message is the complete current snapshot, followed by complete snapshots at greater revisions. There is no cursor. |

`ReportAgentState.state` accepts only `AGENT_STATE_BUSY` and `AGENT_STATE_IDLE`. Pane IDs have the tmux `%` plus decimal-digits form. Adapter and configured provider IDs match `[a-z0-9][a-z0-9._-]{0,63}`; provider ID `unknown` is reserved.

### `AgentStateSnapshot`

| Field | Type | Meaning |
| --- | --- | --- |
| `through_event_version = 1` | `uint64` | Highest durable event version included in the projection |
| `server_time_ms = 2` | `int64` | Snapshot load time |
| `monitor_health = 3` | repeated `AgentMonitorHealth` | Current component health |
| `panes = 4` | repeated `AgentPaneState` | Current live agent incarnations |

`AgentMonitorHealth` contains `component = 1`, `state = 2`, `reason_code = 3`, and `observed_at_ms = 4`.

`AgentPaneState` contains:

| Field | Number | Type |
| --- | ---: | --- |
| `pane_id` | 1 | `string` |
| `tmux_target` | 2 | `string` |
| `session_name` | 3 | `string` |
| `window_index` | 4 | `uint32` |
| `pane_index` | 5 | `uint32` |
| `pane_pid` | 6 | `uint32` |
| `agent_pid` | 7 | `uint32` |
| `agent_started_at_ms` | 8 | `int64` |
| `provider_id` | 9 | `string` |
| `provider_display_name` | 10 | `string` |
| `working_directory` | 11 | `string` |
| `state` | 12 | `AgentState` (`Busy`, `Idle`, or `Unknown`) |
| `last_transition_at_ms` | 13 | `int64` |
| `work_summary` | 14 | `optional string` |

The pane message does not expose adapter IDs, classifier IDs, evidence provenance, raw screen text, internal summary timestamps, or search behavior. The service has no search RPC or query field. Consumers perform search locally over the snapshots they already hold.

### RPC failure statuses

| Condition | Status |
| --- | --- |
| `ReportAgentState` has an invalid/unknown state, malformed pane ID, or malformed adapter ID | `INVALID_ARGUMENT` |
| The pane does not resolve to a live configured agent incarnation | `FAILED_PRECONDITION` |
| Inventory acquisition, durable append, or monitor runtime is unavailable | `UNAVAILABLE` |
| `TurnComplete` durable append fails | `INTERNAL` |

## Configuration

Optional Claude dashboard generation is configured separately under `[activity_summary]`; see the [complete settings reference](activity-summaries.md#configuration).

Default monitor configuration:

```toml
[agent_monitor]
inventory_interval_ms = 1000
screen_interval_ms = 500
hook_grace_ms = 2000

[[agents]]
id = "codex"
display_name = "Codex"
command_contains = ["codex"]
busy_all = ["Working", "esc to interrupt"]
idle_all = ["Ask Codex to do anything"]
summary_line_prefixes = ["›"]
screen_adapter = "codex-v1"
screen_history_lines = 2000
```

| Key | Constraint and behavior |
| --- | --- |
| `agent_monitor.inventory_interval_ms` | Positive integer; scheduled inventory period |
| `agent_monitor.screen_interval_ms` | Positive integer; scheduled visible-screen period |
| `agent_monitor.hook_grace_ms` | Non-negative lifecycle precedence period |
| `agents[].id` | Unique bounded identifier; `unknown` is reserved |
| `agents[].display_name` | Non-empty display label |
| `agents[].command_contains` | At least one non-empty fragment; any fragment matches case-insensitively |
| `agents[].busy_all` | Optional conjunctive, case-sensitive visible-grid fragments |
| `agents[].idle_all` | Optional conjunctive, case-sensitive visible-grid fragments and idle-placeholder rejection clauses |
| `agents[].summary_line_prefixes` | Optional exact, case-sensitive safe submitted-input prefixes for `generic-v1`; `codex-v1` recognizes its own styled `>` and `›` blocks |
| `agents[].screen_adapter` | `generic-v1` when omitted; accepts `generic-v1` or `codex-v1`; unknown names fail startup |
| `agents[].screen_history_lines` | Integer from 1 through 10,000; defaults to 2,000 history rows before the visible grid |

Process selection prefers a matching process in the pane TTY's foreground process group. Otherwise it selects the shallowest matching descendant of the pane root, with PID as a deterministic tie-breaker. Multiple named provider matches produce provider `unknown` rather than choosing configuration order. Missing trustworthy process start time degrades inventory and does not create an incarnation.

The shipped named defaults cover Codex, Claude, and OpenCode state markers. Codex explicitly selects `codex-v1`. Claude uses `generic-v1` with its configured summary prefix; it has no Claude-specific styled parser. OpenCode uses `generic-v1` without `summary_line_prefixes`, so screen acquisition supplies state but no fallback summary. Its opt-in lifecycle plugin can still send explicit summaries. The `screen_adapter` setting defaults to `generic-v1`; selecting a provider ID alone does not select a parser.

See [provider screen adapters](screen-adapters.md) for capture timing, incarnation baselines, prompt selection, and provider limitations.

## Privacy and field bounds

The capture port invokes `tmux capture-pane` for an already identified pane. Visible classification starts at row `0`; conditional recovery preserves styles and starts at the negative configured history depth. Raw captures remain within the capture and adapter boundary and are not stored or logged, including on failure. Adapters return typed state and ordered prompt fingerprints with optional normalized candidates. The runtime retains only fingerprints in its ephemeral acquisition checkpoint and forwards at most one eligible summary through the existing screen event.

Before durable append or publication, tmux-derived metadata is terminal-sanitized and bounded:

| Value | Maximum Unicode scalar values |
| --- | ---: |
| tmux target, session name, provider display name | 256 |
| working directory | 1024 |
| work summary | 160 |
| health component | 64 ASCII identifier bytes |
| health reason code | 160 ASCII identifier bytes |

## Health and failure behavior

The public snapshot reports `inventory` and `screen` health after a component first degrades or subsequently changes/recoveries. Initial success does not create a health row. Repeated identical health is deduplicated.

| Reason code | Source |
| --- | --- |
| `command_unavailable` | Required inventory command is absent |
| `command_failed` | Inventory command exits unsuccessfully |
| `malformed_output` | Inventory output cannot be parsed completely |
| `missing_start_time` | A selected agent process lacks trustworthy start time |
| `capture_unavailable` | Screen-capture command is absent |
| `capture_failed` | Screen capture exits unsuccessfully |
| `pane_departed` | Pane disappears during capture |
| `timeout` | Acquisition exceeded the bounded deadline or a prior acquisition still owns its gate |
| `task_failed` | The bounded acquisition worker could not start or return |
| `ok` | Recovery to healthy |

An inventory failure preserves current panes and never infers mass departure. A screen failure preserves lifecycle state and the prior fallback. Projector failures leave the checkpoint and projection unchanged and are retried by the event-handler loop. A schema initialization, startup catch-up, or snapshot-load error prevents the server from becoming ready.

## Lifecycle limits

- `WatchAgentStates` is snapshot-then-stream, not cursor replay. Slow consumers may coalesce obsolete in-memory snapshots; reconnecting restores the latest complete state.
- Provider screen markers and styled parsing are version-sensitive. Inconclusive text is preserved as uncertainty; `codex-v1` does not establish a Claude-specific rendering guarantee.
- History recovery is bounded and starts with a baseline that emits no existing prompt. Lost fingerprint overlap also establishes a new baseline without adopting its contents. These rules can leave late-attached or older work without a recovered summary.
- Harold does not infer busy/idle from CPU use, tmux window activity, or elapsed silence.
- Harold does not navigate tmux for the dashboard and does not implement dashboard search.
- The OpenCode lifecycle plugin is opt-in and is not installed by `make deploy`; its screen provider has no fallback-summary prefix.
- The `TurnComplete` RPC is the notification ingress. Empty prompts preserve summaries and cannot explicitly clear them.

For operational registration and verification, see [Set up agent monitor hooks](../../how-tos/setup-agent-monitor-hooks.md). For rationale, see [Harold Architecture](../../explanations/architecture.md).
