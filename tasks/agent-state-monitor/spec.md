# Agent State Monitor Backend

## Status

Implemented behavior contract, reconciled with corrective commit `1568fd0`. Current-code completion review, offline Task 12 gates, and the authorized same-store `50061` startup/replay proof passed. Claude sequential and three-provider concurrency remain externally blocked and are not claimed as passed.

## Problem Framing

Harold can discover configured agent processes in tmux panes and receives a durable `TurnCompleted` event, but it cannot answer the current operational questions: which agent panes are busy, idle, or not yet classifiable, and what concise task is each agent currently working on?

The answer currently has no single owner. Tmux inventory, agent lifecycle hooks, visible-pane evidence, durable events, and live consumers would become tangled if each adapter mutated shared state independently. Maintainers need to be able to answer:

- Which component accepts each input?
- Which facts are durable?
- How is conflicting evidence reconciled?
- Where is current state stored and rebuilt?
- What does a late or reconnecting consumer receive?

Use these terms consistently:

- **observation**: a factual input from inventory, a lifecycle hook, or screen classification.
- **agent incarnation**: one matched agent process identified by pane ID, pane-root PID, matched agent PID, agent process start time, and provider ID.
- **effective state**: Harold's reconciled `Busy`, `Idle`, or `Unknown` answer for a live pane.
- **work summary**: the sanitized, bounded most recent substantive submitted user instruction, retained as explicit lifecycle/completion and provider-specific screen candidates with durable observation timestamps. The latest candidate is effective, with explicit winning an equal-timestamp tie. Adapters may scan several recent user turns to skip empty, system, tool-result, and UI-placeholder entries, but do not synthesize or concatenate them.
- **projection**: current agent state stored in Harold's application-state database.
- **snapshot**: the complete projection sent to a consumer at one event-stream version.

`EventStream` means Harold's durable ordered domain-event log. It is distinct from the request-progress-shaped `events::StreamEvent` type.

## Goal

Make Harold the authoritative tmux-agent monitor. Harold will inventory tmux panes, identify configured agent processes, classify visible-screen evidence, receive lifecycle observations and concise work summaries, append meaningful facts to its durable `EventStream`, project those facts into current application state, and expose a snapshot-then-stream gRPC API. Dashboard users see what each agent is doing, not the provenance used to reconcile its state or summary.

## Non-Goals

- Do not persist raw pane contents, prompts, or terminal history for monitoring.
- Do not treat CPU use, tmux window activity, or elapsed silence as proof of busy or idle.
- Do not add cursor-based event replay to the state-watching RPC in this task.
- Do not route dashboard navigation through Harold; the invoking client owns that tmux effect.
- Do not add a search RPC, search index, or query language to Harold. Search is dashboard-local over fields already present in snapshots.
- Do not replace Harold's existing notification or inbound-message behavior.
- Do not force agent state into the unrelated request-progress `StreamEvent` schema.
- Do not claim exactly-once external notification delivery.

## Update Type

- Primary: new system feature and public gRPC/storage contract.
- Secondary: internal application-projector restructuring.
- Likely permanent documentation: agent-monitor reference plus hook-configuration how-to.
- Existing documentation likely affected: Harold architecture, configuration, hook setup, and gRPC reference.

## Pre-implementation Context

The implementation started from this repository state:

- `harold/src/inbound/tmux.rs` already snapshots `ps`, walks descendants from each `pane_pid`, and matches configured command fragments.
- `harold/src/settings.rs` currently has one flat `agents.command_contains` list, so it detects presence but cannot name providers or configure screen markers.
- `harold/proto/harold.proto` currently exposes only unary `TurnComplete`.
- `harold/src/store.rs` appends `TurnCompleted` and `InboundMessageReceived` to one ordered `harold/main` stream and owns `harold-state.db`.
- `harold/src/store.rs::stage_unhandled_events` currently stages every new event into the external-delivery outbox.
- `harold/src/projector.rs` dispatches only `TurnCompleted` and `InboundMessageReceived`; unknown types become undeliverable.
- Harold does not instantiate `events::EventsRuntime`, `NotificationsStore`, or its progress-event broadcast loop.

## Behavior Contract

### Inputs

1. One `AgentMonitorRuntime` owns the inventory schedule and is the sole appender for agent-related events. It polls tmux and a process table that includes PID, parent PID, executable command, and process start time. It selects the configured agent process, not merely tmux's long-lived pane-root process.
2. Inventory appends `AgentPaneObserved` only when a live agent incarnation is new or its relevant metadata changes. It considers departure only after two consecutive complete successful inventories; immediately before appending `AgentPaneDeparted`, the runtime revalidates that exact incarnation against a fresh process lookup. A still-live incarnation or failed revalidation never produces departure.
3. The screen adapter captures only the current visible screen of incarnations already identified as agents. It must not pass a negative `capture-pane -S` history offset. Provider-specific state classifiers independently produce `Busy`, `Idle`, or inconclusive, while provider-specific summary extractors independently produce one sanitized bounded work-summary candidate or inconclusive. A useful summary remains valid when state is inconclusive, and a useful state remains valid when summary extraction is inconclusive. The adapter returns both optional facts with the incarnation captured. Raw captured text never enters an event payload, application-state row, API message, diagnostic, or application log.
4. A new unary `ReportAgentState` RPC accepts explicit `Busy` or `Idle` lifecycle observations and an optional work-summary update. It sends a typed command to `AgentMonitorRuntime` and awaits its durable result. The runtime resolves the complete current incarnation and appends `AgentPaneObserved` immediately before `AgentLifecycleObserved` in one ordered `EventStream::append` batch. Re-observation is projection-idempotent. Unresolved agents return `FAILED_PRECONDITION` without an event.
5. Existing `TurnCompleted` events remain durable notification facts. Ingress sends them through the same runtime. The provider hook scans recent transcript user turns and places only the most recent substantive submitted instruction in `TurnCompleteRequest.last_user_prompt`; it excludes the current input composer, empty/system/tool-result entries, and provider UI placeholders. Harold derives a sanitized 160-scalar work-summary update from that field; because this is a non-optional legacy proto3 string, normalized empty maps to `Unchanged`, never `Clear`, while substantive non-empty input maps to `Set`. An exact configured placeholder clears the raw prompt and maps to `Unchanged` before serialization. No second stop-hook request field is introduced, and existing notification fields retain their compatibility bounds. When the agent incarnation resolves, the runtime appends `AgentPaneObserved` immediately before `TurnCompleted`; the stored completion includes the resolved incarnation and derived summary update, providing authoritative idle and non-destructive summary evidence. A completion whose incarnation cannot be resolved still notifies but does not alter agent state. Missing, `unknown`, stale, or removed provider identity selects conservative all-configured exact-placeholder matching.

### Reconciliation

- `AgentIncarnation` is `(pane_id, pane_pid, agent_pid, agent_started_at_ms, provider_id)`. Every pane-scoped agent event carries the full value. Replacing Claude with Codex, or restarting the same provider process inside a long-lived shell pane, creates a new incarnation and clears prior hook/screen evidence.
- Inventory owns presence. Lifecycle and screen events apply only when their complete incarnation matches the current projection row; delayed observations for a departed incarnation are retained in history but ignored by current state.
- A hook observation wins for a configurable two-second grace period so the terminal can repaint.
- After the grace period, a later conclusive screen observation may repair a missed or stale hook transition.
- Within one captured screen, a strong busy marker wins if both busy and idle-looking markers are present.
- An inconclusive screen state never changes effective state, but it does not block a useful fallback-summary observation from being appended and projected.
- With no conclusive lifecycle or screen evidence, effective state is `Unknown`.
- Each incarnation maintains an explicit work-summary candidate and a screen-extracted candidate with separate durable observation timestamps. The most recently observed substantive candidate is effective; explicit wins an equal-timestamp tie. A new incarnation starts with neither candidate and cannot inherit either from the previous process.
- Summary normalization strips C0, C1, and ESC control sequences, collapses Unicode whitespace to single spaces, trims the result, and truncates to 160 Unicode scalar values before an event is appended. Discarded content and raw captures are never logged.
- For `ReportAgentState`, an absent `work_summary` means no explicit-summary update, a present value that normalizes to empty explicitly clears the explicit candidate, and a present non-empty normalized value replaces it unless it exactly equals a configured idle placeholder, which maps to `Unchanged` before serialization. A non-empty normalized `TurnCompleteRequest.last_user_prompt` replaces the explicit candidate unless it is an exact configured placeholder; an exact match clears the legacy raw prompt and serializes `Unchanged`. A normalized-empty legacy prompt also means `Unchanged` so legacy or partial stop hooks cannot erase the task.
- Screen extraction is fallback acquisition, not a parallel display field. An inconclusive or exact idle-placeholder extraction does not clear or refresh the screen candidate. A later substantive Busy screen prompt may replace an older retained explicit completion summary; a subsequent Idle observation without a substantive summary retains that current instruction.
- Provider-specific screen extraction calls the same work-summary normalizer and 160-scalar bound as explicit input. The acquisition result contains independent optional state and summary outputs. The runtime compares each conclusive `Some` value with its last successfully appended meaningful value, converts unchanged values to `None`, and appends `AgentScreenObserved` when at least one changed value remains. Screen-state deduplication is scoped to a lifecycle reconciliation epoch: accepting a lifecycle observation invalidates the state dedupe for that incarnation; a conflicting screen state sampled during hook grace is retained as pending but omitted from the durable event, then appended once after grace if it remains visible. This single post-grace revalidation is meaningful even when its state text matches the prior epoch; later identical samples are deduplicated normally. Summary changes remain independent and may be appended during grace. Thus the durable event carries `state: Option<BusyOrIdle>` and `fallback_summary: Option<String>` plus `observed_at_ms`, including `state = None` with a changed useful summary. Each present field reaches current state only through normal EventStream projection; an absent field preserves its corresponding projected candidate. If both are absent after deduplication, no event is appended. There is no transient publication shortcut.
- `Unchanged` preserves both the explicit summary and its timestamp. `Set` replaces both even when the normalized text matches the previous value. A repeated identical screen candidate is deduplicated and does not refresh its timestamp; a changed candidate replaces both screen fields.
- Current acquisition and ingress reject only exact equality with a normalized configured idle-placeholder fragment. A currently configured provider ID selects only its own fragments; another provider's exact placeholder remains legitimate. A missing, `unknown`, stale, or removed provider ID conservatively selects every configured provider's fragments. Lifecycle and completion matches become non-setting before event serialization, so no ingress/repair pair can straddle a projection page. To repair exact placeholders already retained from historical events, startup first projects all history without a hub, collects every required `AgentWorkSummaryCandidatesRepaired` fact into one append, projects that batch, and only then creates the hub and runtime seed. Each repair carries full incarnation, independent explicit/screen clear flags, typed `ConfiguredIdlePlaceholder` reason, and observation time, but no rejected text. The reducer clears each marked candidate with its timestamp and recomputes the effective summary; stale-incarnation and all-false repairs are ignored.
- The reducer changes `last_transition_at` only when effective state changes. Duplicate observations may advance observation metadata but do not create a false transition.
- `AgentPaneDeparted` removes the pane from the current projection. Historical events remain durable.
- Inventory/screen deduplication advances only after its observation event append succeeds; a failed append must be eligible for retry on the next poll.
- Acquisition adapters submit typed commands to the single `AgentMonitorRuntime`; they never append agent events directly. Lifecycle RPCs await a oneshot reply from that runtime, while polling and screen work cannot interleave stale append decisions around a lifecycle batch.

### Projection and publication

- The application projector reads durable events in `EventStreamVersion` order.
- In one application-state transaction it applies agent events, stages only externally deliverable events, and advances the application checkpoint.
- The stored projection records the highest durable event-stream version it includes.
- Only after commit does Harold publish a complete `AgentStateSnapshot` to live subscribers.
- The publisher retains its latest value even with zero subscribers, using `watch::Sender::send_replace` or an equivalent owned receiver; losing all consumers must not discard current in-memory state.
- On startup Harold catches the projection up to the durable stream head without a publisher, appends all required legacy-candidate repairs in one batch, projects them completely, loads the clean snapshot, creates the hub, seeds the monitor runtime, and only then reports the watch service ready. Any catch-up, repair, or snapshot-load failure aborts startup.
- Shutdown closes every open watch stream so Tonic graceful shutdown cannot wait indefinitely for dashboard consumers.
- Inventory and screen adapters append deduplicated `AgentMonitorHealthChanged` events when a component degrades or recovers. Health changes therefore have durable ordering and can advance the public snapshot revision without waiting for an unrelated pane event.

### Consumer connection

- `WatchAgentStates` sends the complete current snapshot first and then snapshots with a greater `through_event_version`.
- A late, slow, or reconnecting consumer does not need prior in-memory messages; it starts from current stored state.
- Each pane exposes at most one presence-aware `work_summary`. Proto3 absence means Harold has no usable summary; dashboard copy for that value is exactly `No work summary reported`. No summary timestamp is exposed because the dashboard has no current consumer requirement.
- The dashboard performs filtering and search locally over snapshot fields, including `work_summary`. Harold does not accept or persist a search term.
- The MVP does not accept a last-event cursor. A future event-oriented API may resume from `EventStreamVersion` or resolve an event UUID to its version.

## Contract Surface

Public and cross-module contract changes are required.

### Durable events

| Name | Fields | Producer | Projection effect |
| --- | --- | --- | --- |
| `AgentPaneObserved` | full agent incarnation, tmux target, session/window/pane indices, working directory, observed time | monitor runtime | Upsert live incarnation metadata; default a new incarnation to `Unknown` |
| `AgentPaneDeparted` | full agent incarnation, observed time | monitor runtime after live revalidation | Remove only the matching current incarnation |
| `AgentLifecycleObserved` | full agent incarnation, `Busy` or `Idle`, adapter ID, explicit summary update (`Unchanged`, `Clear`, or sanitized `Set`), observed time | monitor runtime for hook RPC | Apply hook evidence and the explicit summary update only to the matching current incarnation |
| `AgentScreenObserved` | full agent incarnation, optional `Busy`/`Idle` state, optional sanitized fallback summary, classifier/extractor ID/version, observed time | monitor runtime for screen classifier/extractor | Apply each present fact independently to the matching incarnation; an absent fact preserves its corresponding candidate; both absent produces no event |
| `AgentWorkSummaryCandidatesRepaired` | full agent incarnation, independent `clear_explicit`/`clear_screen` flags, `ConfiguredIdlePlaceholder` reason, observed time | startup repair gate and runtime legacy-candidate defense | Clear each marked legacy candidate and its timestamp for the matching incarnation; recompute by normal candidate recency; never stage external delivery |
| `AgentMonitorHealthChanged` | component, `Healthy` or `Degraded`, bounded reason code, observed time | inventory/screen runtime | Update stored monitor health and snapshot revision |
| `TurnCompleted` | existing notification payload plus optional resolved agent incarnation and a derived sanitized `Unchanged`/`Set` summary update from legacy `last_user_prompt` | existing stop hook through monitor runtime | Preserve notification behavior; reconcile idle and apply only substantive, non-placeholder summary input for a matching incarnation |

Repeated inventory and screen polls that produce no meaningful change do not append events.

### Stored application state

`agent_panes` is owned by Harold's application projector and contains:

| Field | Meaning |
| --- | --- |
| `pane_id` | Stable pane key within the current tmux server, such as `%22` |
| `tmux_target` | Human-readable `session:window.pane` target |
| `session_name`, `window_index`, `pane_index`, `pane_pid` | Navigation and pane-root metadata |
| `agent_pid`, `agent_started_at_ms`, `provider_id` | Matched agent-process identity; together with pane fields, the incarnation key |
| `working_directory`, `provider_display_name` | Display and classifier selection metadata |
| `hook_state`, `hook_observed_at_ms` | Latest lifecycle evidence |
| `screen_state`, `screen_classifier_id`, `screen_observed_at_ms` | Latest conclusive screen evidence and classifier version; never captured text |
| `effective_state`, `effective_source` | Reconciled current answer and internal provenance; provenance is not published to the dashboard |
| `explicit_work_summary`, `explicit_work_summary_updated_at_ms` | Sanitized explicit candidate and its lifecycle/completion observation time; both nullable and cleared together |
| `screen_work_summary`, `screen_work_summary_updated_at_ms` | Sanitized provider-specific fallback observation and its screen observation time; both nullable and updated together through the durable screen event path |
| `work_summary` | Effective concise summary; nullable and bounded to 160 Unicode scalar values |
| `last_transition_at_ms` | Time effective state last changed |
| `last_event_version` | Durable event version that last changed this row |

`agent_monitor_health` stores the latest health state, bounded reason code, observation time, and last event version for each monitor component. It never stores raw command output.

Monitor component and reason codes use the same bounded ASCII identifier grammar as adapter IDs. Human-readable command output stays in sanitized, bounded operational logs and never enters the public snapshot.

The application checkpoint remains the snapshot's `through_event_version`. Migrations are checksum-tracked and additive; existing event data is not rewritten.

### Configuration

Replace the flat matcher with named provider definitions:

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
```

Claude and OpenCode receive equivalent defaults verified against their installed versions. Additional providers use the same named contract. Ambiguous command matches retain the pane as an agent with provider `Unknown` and emit an operator warning rather than silently choosing by configuration order.

`command_contains` matches when any fragment occurs case-insensitively in the executable command. Inventory prefers a matching process in the pane TTY's foreground process group, otherwise the shallowest matching descendant of the pane root; PID breaks equal-depth ties deterministically. The selected process PID plus OS-reported start time prevents PID reuse and same-pane agent replacement from inheriting evidence.

`busy_all` and `idle_all` are conjunctive clauses for state: every listed fragment must occur somewhere in the normalized current visible grid, with no same-line requirement. `summary_line_prefixes` is provider-specific; the extractor scans recent matching submitted-input lines from bottom to top, removes the configured prefix, rejects only a candidate exactly equal to a normalized configured idle fragment, and accepts the first substantive candidate that remains non-empty after work-summary normalization. A real instruction that merely mentions the placeholder phrase remains valid. The extractor does not combine multiple prompts. Normalization converts CRLF to LF and removes C0, C1, and ESC control sequences while preserving printable Unicode. Screen matching is case-sensitive; if both state clauses match, busy wins. Alternative any/all groups and regular-expression extraction are deferred until a provider demonstrates the need.

Existing flat `[agents].command_contains` configuration remains loadable during migration. It creates unnamed presence matchers with no screen classification; operators receive a deprecation warning and can migrate provider-by-provider.

### gRPC

Conceptual protobuf contract:

```proto
enum AgentState {
  AGENT_STATE_UNSPECIFIED = 0;
  AGENT_STATE_BUSY = 1;
  AGENT_STATE_IDLE = 2;
  AGENT_STATE_UNKNOWN = 3;
}

enum MonitorHealthState {
  MONITOR_HEALTH_STATE_UNSPECIFIED = 0;
  MONITOR_HEALTH_STATE_HEALTHY = 1;
  MONITOR_HEALTH_STATE_DEGRADED = 2;
}

rpc ReportAgentState(ReportAgentStateRequest) returns (ReportAgentStateResponse);
rpc WatchAgentStates(WatchAgentStatesRequest) returns (stream AgentStateSnapshot);

message ReportAgentStateRequest {
  string pane_id = 1;
  AgentState state = 2;
  string adapter_id = 3;
  optional string work_summary = 4;
}

message ReportAgentStateResponse {
  bool accepted = 1;
}

message WatchAgentStatesRequest {}

message AgentPaneState {
  string pane_id = 1;
  string tmux_target = 2;
  string session_name = 3;
  uint32 window_index = 4;
  uint32 pane_index = 5;
  uint32 pane_pid = 6;
  uint32 agent_pid = 7;
  int64 agent_started_at_ms = 8;
  string provider_id = 9;
  string provider_display_name = 10;
  string working_directory = 11;
  AgentState state = 12;
  int64 last_transition_at_ms = 13;
  optional string work_summary = 14;
}

message AgentMonitorHealth {
  string component = 1;
  MonitorHealthState state = 2;
  string reason_code = 3;
  int64 observed_at_ms = 4;
}

message AgentStateSnapshot {
  uint64 through_event_version = 1;
  int64 server_time_ms = 2;
  repeated AgentMonitorHealth monitor_health = 3;
  repeated AgentPaneState panes = 4;
}
```

Existing `TurnCompleteRequest` and field numbers 1 through 5 remain unchanged. `ReportAgentStateRequest.work_summary = 4` uses proto3 explicit presence: absent means `Unchanged`, present and empty after normalization means `Clear`, and present and non-empty means `Set(normalized_value)`. Adapter and provider identifiers must match `[a-z0-9][a-z0-9._-]{0,63}`. Harold passes the request to the serialized monitor runtime, which resolves the complete agent incarnation and display metadata through its single inventory port rather than trusting the hook. `AgentPaneObserved` and `AgentLifecycleObserved` are always appended together in that order. The existing loopback default remains the security boundary.

RPC success means the lifecycle observation is durably appended. It does not claim that the asynchronous application projection is already visible.

`AgentStateSnapshot` contains `through_event_version`, server time, monitor health, and repeated pane records. Each pane record contains navigation/display fields, effective state, and `optional work_summary`; it does not expose a summary timestamp, evidence source, classifier ID, adapter ID, raw screen text, or search behavior. When `work_summary` is absent, the dashboard renders exactly `No work summary reported`. Before persistence or publication Harold strips C0, C1, and ESC control sequences from tmux-derived display strings and enforces explicit field bounds: 256 Unicode scalar values for targets/provider labels, 1024 for working directories, and 160 for work summaries. Truncation is recorded only as a bounded diagnostic fact without logging discarded content.

Every pane record explicitly carries the full incarnation identity: `pane_id`, `pane_pid`, `agent_pid`, `agent_started_at_ms`, and `provider_id`. The API never asks a consumer to infer identity from display fields.

Generated protobuf messages and tonic client/server types move into a small `harold-api` workspace library. Harold and the separate dashboard share that library; the dashboard does not copy the protobuf contract or import Harold application internals.

## Architecture

```text
Codex / Claude / OpenCode hooks ── state + summary ──┐
                                                     │
tmux + process table ── inventory schedule ──────────┼─ AgentMonitorRuntime
                                                     │  ├─ sole agent-event appender
visible pane ── state + summary fallback ───────────┘  └─ departure revalidation
                                                                │
                                                                ▼
                                                     durable EventStream
                                                         │
                                                         ▼
                                              ApplicationProjector
                                              ├─ pure state reducer
                                              ├─ agent_panes projection
                                              ├─ delivery_outbox
                                              └─ application checkpoint
                                                         │ commit
                                                         ▼
                                              current snapshot publisher
                                                         │
                                                         ▼
                                               WatchAgentStates gRPC
```

Adapters acquire typed inputs but do not append. `AgentMonitorRuntime` serializes agent-event decisions, owns deduplication, and revalidates departure. The pure reducer owns evidence precedence. The projector is the only application-state writer. It continues treating unknown non-agent event types as poison events rather than silently skipping them. The publisher distributes committed state and is never a substitute for the durable log.

## Failure Modes

| Failure mode / trigger | Expected behavior | Persistence effect | Feedback and verification |
| --- | --- | --- | --- |
| `tmux` or `ps` command fails | Mark monitor health degraded and retain the last projection; do not infer that every pane departed | Append one deduplicated health transition; no departure events | Warning with command and exit status; integration test |
| A matched process has no trustworthy start time | Omit that candidate from the public inventory rather than construct a reusable identity; mark inventory health degraded | Append one deduplicated health transition; no pane observation or departure inferred from the incomplete scan | Bounded reason code; process-metadata fixture and recovery test |
| Pane disappears between inventory and capture | Skip its screen sample; the next complete inventory decides departure | No false screen event | Debug/warning without loop failure; race test |
| Delayed observation names an old agent incarnation | Keep the durable history fact but ignore it for current state | No mutation of the new incarnation | Old-incarnation lifecycle/screen/departure tests |
| Agent process/provider changes inside the same shell pane | Create a new incarnation and clear old evidence | Upsert new process identity with `Unknown` until new evidence | Rapid Claude-to-Codex and same-provider restart tests |
| Provider matches multiple definitions | Keep pane visible as provider `Unknown` | Observation records unknown provider | Operator warning; config/matcher test |
| Screen capture fails | Preserve lifecycle state and fallback summary | No screen event | Health/evidence diagnostics; adapter test |
| State classification is inconclusive but summary extraction succeeds | Preserve screen state and durably apply the bounded fallback summary | Append `AgentScreenObserved { state: None, fallback_summary: Some(...) }` when the summary changes | Independent-output classifier test |
| Summary extraction is inconclusive but state classification succeeds | Preserve fallback summary and durably apply the state | Append `AgentScreenObserved { state: Some(...), fallback_summary: None }` when state changes | Independent-output classifier test |
| Both screen outputs are inconclusive | Preserve both candidates | No screen event | Classifier test |
| Explicit work summary is absent | Preserve the explicit candidate and choose the most recently observed substantive candidate | Lifecycle event records `Unchanged` | RPC presence and reducer tests |
| Present `ReportAgentState.work_summary` normalizes to empty | Clear the explicit candidate and expose the screen fallback, or make the effective summary absent if no fallback exists | Lifecycle event records `Clear` | Sanitizer and projection tests |
| Legacy completion prompt normalizes to empty | Preserve the explicit summary so legacy/partial stop hooks cannot erase it | `TurnCompleted` records `Unchanged` | Completion ingress and replay tests |
| Screen summary extraction is inconclusive and no state change remains after deduplication | Preserve the existing screen candidate and state; never inspect history | No screen event | Extractor fixture test |
| Internal summary candidate and its observation timestamp become inconsistent | Fail the projection-row load as an internal storage invariant violation; retain the last committed publication and never emit the affected pane | No inconsistent snapshot is published | Repository migration and snapshot-load tests |
| Summary contains controls, excessive whitespace, or more than 160 scalars | Normalize before append; never log discarded or raw content | Only normalized bounded text is durable | Adversarial ingress and event-payload tests |
| Historical explicit or screen candidate exactly equals a configured idle placeholder | Append an incarnation-scoped projection-only repair that independently clears each affected candidate and timestamp | Durable repair prevents full replay from resurrecting the placeholder; no outbox entry | Replay, retry/dedupe, stale-incarnation, and same-store restart checks |
| Startup repair append or projection fails | Abort startup before creating the hub/runtime seed; the next startup retries from durable state | No published partial clear and no lost repair | Fault-injected startup/runtime tests |
| One screen contains busy and idle markers | Classify busy because an active progress marker is stronger than a passive prompt marker | Append busy evidence only if changed | Mixed-marker classifier fixture |
| Busy/idle hook is missed | A later conclusive screen observation may repair state after the hook grace period | Append screen observation only on evidence change | Reducer precedence test |
| Hook and freshly repainted screen conflict | Hook wins during grace; later conclusive screen evidence may supersede it | Both facts remain ordered | Clock-controlled reducer test |
| Invalid pane ID or `Unknown` submitted to lifecycle RPC | Reject with `INVALID_ARGUMENT` | No event | RPC validation test |
| Valid pane ID cannot resolve to a live configured agent incarnation | Reject with `FAILED_PRECONDITION` | No event | RPC race/resolution test |
| Lifecycle and departure decisions race in either order | Serialize through the runtime; revalidate before departure append; lifecycle appends observe then state | Final projection matches the live process in both append orders | Hook-before-departure and departure-before-hook integration tests |
| Event append fails | Return unavailable/internal status; hook wrapper logs and exits without blocking the agent | No partial projection | Fault-injection test |
| Projector stops after append | Replay from stored checkpoint on restart | Durable event remains unapplied until recovery | Restart test |
| Projection transaction fails | Roll back projection, outbox staging, and checkpoint together | No partial application | Transaction failure test |
| Commit succeeds before live publication and Harold exits | Consumers disconnect; after restart they receive the committed stored snapshot | Projection remains correct | Restart/reconnect test |
| Watcher joins late or reconnects | Send latest full snapshot, then later revisions | No mutation | Streaming integration test |
| Watcher is slow or drops | Coalesce obsolete in-memory snapshots; reconnect restores current state | No mutation | Slow-consumer test |
| No dashboard is connected | Monitoring, event append, and projection continue | State remains current | No-subscriber integration test |
| Dashboard searches or clears a search term | Filter its local snapshot only; Harold receives no query and emits no event | No mutation | Protobuf surface assertion and dashboard-owned test |
| Harold shuts down with an open watch RPC | Close the stream and complete graceful shutdown | Stored state remains current | Shutdown integration test |
| Monitor component degrades or recovers without a pane change | Append one health transition and publish the resulting revision | Health projection changes; pane rows do not | Health dedup/recovery streaming test |

## Test Theories

- **Inventory is conservative:** table-driven `ps` trees cover direct processes, descendants, wrappers, ambiguous providers, missing process start times, malformed command output, and full-scan failure without false departures.
- **Agent incarnations do not inherit evidence:** rapid provider replacement, same-provider restart, pane PID reuse, and matched agent PID reuse with a different start time all start unknown and cannot reuse departed evidence.
- **Screen classifiers are provider-local:** fixtures for busy, idle, ANSI/Unicode output, cropped screens, inconclusive screens, and marker conflicts never persist raw text.
- **Work summaries select durable recency:** optional-field presence, explicit clear only through `ReportAgentState`, non-destructive legacy completion empties, independent provider-specific screen extraction, equal-timestamp explicit ties, incarnation replacement, and restart replay converge on one sanitized effective summary.
- **Summary recency and repair are durable:** candidate timestamps choose the latest substantive instruction with explicit winning ties; historical exact configured placeholders produce typed repair facts that survive projection rebuild, while new exact-placeholder ingress becomes non-setting before serialization. Removed-provider startup, unknown-provider lifecycle, resolved/tracked-unresolved unknown completion, known-provider cross-provider isolation, and legitimate containing text cover the provider-selection boundary.
- **Reducer precedence is deterministic:** a fake clock covers unknown startup, hook grace, later screen repair, duplicate observations, and departure.
- **Projection is atomic and replayable:** event batches update agent rows, delivery outbox, and checkpoint together; reopening from each injected failure point converges to the same snapshot.
- **Lifecycle/departure ordering converges:** every accepted lifecycle call creates one ordered pane-plus-lifecycle batch, all agent decisions share one appender, and departure is live-revalidated; both race orders converge to actual presence.
- **Existing effects remain selective:** `TurnCompleted` and `InboundMessageReceived` still stage their external effects; agent-only events never become undeliverable outbox work.
- **Streaming starts with truth:** late, slow, disconnected, and restarted consumers always receive a full stored snapshot before later revisions.
- **Subscription has no query gap:** the watch RPC subscribes to the DB-seeded hub and sends that receiver's current value before waiting for changes; it does not query state and subscribe afterward.
- **Health participates in ordering:** degradation and recovery append only on transition and reach consumers even when pane state is unchanged.
- **Published strings are terminal-safe:** adversarial tmux names, paths, identifiers, reason codes, and errors cannot inject terminal control sequences and respect field bounds.
- **Search stays out of Harold:** protobuf contract tests prove there is no query/search RPC or request field and that panes expose only the effective optional summary.

## Acceptance Criteria

- Harold discovers live Codex, Claude, OpenCode, and configured agent panes using named process-tree matchers.
- Current visible panes can contribute provider-specific busy/idle evidence without raw screen persistence.
- Explicit lifecycle/completion input and provider-specific visible-screen extraction provide independently timestamped candidates; the latest substantive candidate is effective, with explicit winning ties.
- Captured pane content never appears in durable events, application-state rows, gRPC messages, or logs.
- Durable events and current projection contain only normalized work summaries of at most 160 Unicode scalar values.
- Exact configured idle placeholders are rejected at current acquisition/ingress and durably repaired when found in historical candidate state; replay cannot resurrect a repaired candidate.
- Delayed events from an old agent incarnation cannot remove or reclassify a newer incarnation.
- Replacing or restarting an agent inside the same long-lived tmux shell clears prior evidence.
- Lifecycle hooks append durable busy/idle facts; existing `TurnCompleted` continues to notify and projects idle when Harold resolves the matching current incarnation.
- Every effective state is `Busy`, `Idle`, or `Unknown`; evidence provenance remains an internal reducer concern and is not exposed in pane snapshots.
- Every pane snapshot contains an optional `work_summary`; its absent dashboard value is exactly `No work summary reported`, and no summary timestamp is exposed.
- Harold exposes no search RPC or search request field; dashboard filtering is local.
- The application-state database can rebuild and serve the current snapshot after Harold restarts.
- `WatchAgentStates` always begins with a full current snapshot and then emits only greater event-stream revisions.
- Monitor degradation and recovery reach connected consumers even when pane state does not change.
- Open watch streams close promptly during Harold shutdown.
- Inventory/screen failures do not erase current state or crash Harold.
- Existing notification and inbound-routing tests continue to pass.
- New dependencies are not fetched or added until the required Rust supply-chain audit approves them.

## Verification Plan

- Observe each new reducer, classifier, projector, and RPC test fail for the intended missing behavior before implementation.
- Run focused Harold tests during each red-green cycle.
- Run `cargo fmt --all -- --check`.
- Run `cargo test --workspace --all-targets` with the repository's required environment.
- Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- Run a release build and the repository's offline advisory/dependency checks.
- Start Harold against a temporary store, connect a test watcher, and verify snapshot-first behavior across disconnect and restart.
- Exercise live tmux inventory and screen classification without printing captured pane content.
- Inspect event payloads, projection rows, structured logs, and protobuf snapshots during an adversarial screen/summary test to prove raw captured content never crosses the acquisition boundary.
- Obtain the required independent completion review and resolve every finding.

## Documentation Notes

After implementation, migrate verified material into:

- a reference for agent event, projection, configuration, and gRPC contracts;
- a how-to for Codex, Claude, OpenCode, and custom lifecycle hooks;
- an explanation of hook/screen evidence precedence and state limitations.

The HTML dashboard visual and implementation task breakdown remain temporary task artifacts.

## Risks And Approved Decisions

- Provider screen markers are version-sensitive. Configuration and classifier IDs make drift visible; inconclusive output preserves rather than invents state.
- Work-summary extraction is intentionally deterministic and provider-specific: scan a small recent-user-turn window, skip non-substantive and exact UI-placeholder entries, and select the most recent substantive submitted instruction without synthesis. Durable candidate timestamps determine recency, so a current Busy screen prompt can replace a retained prior completion while Idle placeholder/absence preserves the current summary.
- Tmux pane IDs are stable only for the life of one tmux server. Inventory presence, not historical pane ID alone, controls whether a row is exposed.
- The user approved current-state snapshot recovery for the MVP and deferred last-event cursor replay.
- The user approved deliberate Harold coupling while requiring explicit input, reducer, projection, subscription, and effect boundaries.
- The user approved non-destructive empty semantics for legacy `TurnCompleteRequest.last_user_prompt`, explicit clear only through `ReportAgentStateRequest.optional work_summary = 4`, independent optional state/summary fields in durable `AgentScreenObserved`, public `optional work_summary = 14` without a timestamp, dashboard copy `No work summary reported`, the 160-scalar cap, dashboard-local search, and most-recent-substantive-user-instruction selection without multi-turn synthesis.
