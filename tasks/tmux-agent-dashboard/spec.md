# Tmux Agent Dashboard

## Status

Spec draft. This document is a pre-implementation specification and must be checked against the final code before durable documentation is published.

## Problem Framing

Harold will know the current state of agent panes, but operators need one fast keyboard-driven view across tmux sessions. They should not have to remember session names, inspect every pane, or infer whether an agent is working from terminal noise.

The dashboard must answer, at a glance:

- Which configured agents are busy, idle, or unknown?
- Where is each agent running?
- What is each agent currently working on?
- Is the displayed projection live or stale because Harold disconnected?
- How can the operator jump to the selected pane without leaving the keyboard?

## Goal

Build a Rust Ratatui application that consumes Harold's `WatchAgentStates` stream, renders the latest complete agent projection, preserves a stable selection as snapshots change, and navigates the invoking tmux client to the chosen pane.

The interface follows the industrial signal-board visual reference in [dashboard-visual.html](dashboard-visual.html): state is the dominant signal, information density is deliberate, and every browser-only treatment must reduce cleanly to terminal cells, borders, text, and ANSI colors.

## Non-Goals

- Do not inventory processes, capture panes, classify screens, or reconcile state in the dashboard.
- Do not persist a local cache of Harold's projection.
- Do not replay every agent event or accept a last-event cursor in the MVP.
- Do not edit tmux layouts, send prompts, stop agents, or manage sessions.
- Do not provide mouse-only interactions; keyboard operation is complete.
- Do not make the HTML visual a production web application.

## Update Type

- Primary: new operator-facing TUI and Harold gRPC consumer.
- Secondary: local tmux navigation workflow.
- Likely permanent documentation: installation/configuration how-to, key reference, and connection troubleshooting.
- The HTML file is a temporary design reference, not permanent product documentation.

## Current Context

- The dashboard repository is an unborn, otherwise empty Git repository.
- Rust `1.97.1`, Cargo `1.97.1`, and tmux `3.6a` are installed locally.
- Ratatui and Crossterm are not present in the local Cargo source cache and require the mandated supply-chain audit before addition or download.
- Live verification showed `tmux display-message -p '#{client_name}'` identifies the invoking client and `tmux switch-client -c <client> -t <pane-id>` changes session, window, and pane atomically.
- Harold owns inventory, screen evidence, state reconciliation, durable events, the stored projection, and the streaming API defined by the coordinated backend specification.

## Behavior Contract

### Startup and connection

- Start in the terminal's alternate screen, enable raw mode, and restore both on every normal, error, panic, and signal exit path supported by the terminal backend.
- Default Harold endpoint is `http://127.0.0.1:50060`; `--endpoint` overrides it.
- Capture the invoking tmux client name once at startup. Viewing remains available outside tmux, but navigation is disabled with an explicit explanation.
- Connect to `WatchAgentStates`. The first valid message replaces the loading state with the complete current snapshot.

### Snapshot handling

- The first valid full snapshot on every newly established stream is authoritative and confirms `Live`, even when its revision is equal to or lower than the retained snapshot. This permits recovery after Harold's application store is deliberately replaced or reset. Selection is preserved only for an incarnation present in the new authoritative snapshot.
- After the first snapshot on a connection, accept only a greater `through_event_version`. An exact duplicate is ignored and a lower version is a protocol error.
- Retain `server_time_ms` and every monitor-health entry from each accepted snapshot. A connected stream with any degraded monitor component remains `Live` for transport/revision purposes but must render a prominent `MONITOR DEGRADED` warning; it must never look fully healthy. Existing pane rows remain visible because degraded acquisition does not invalidate Harold's last committed projection.
- Rows are keyed by Harold's full agent incarnation `(pane_id, pane_pid, agent_pid, agent_started_at_ms, provider_id)`. If the selected incarnation remains present, preserve it across revisions and sorting. Agent replacement or restart inside the same pane is a new row and cannot inherit selection. If the selected incarnation disappears, select the next row at its former index, then the preceding row, or none when the list is empty.
- Default ordering is `Busy`, `Idle`, `Unknown`, then session name, window index, and pane index.
- The footer shows Harold connection state and the applied event-stream version.

### Search

- Search is a dashboard-local filter over the latest authoritative snapshot. It does not issue a Harold query or pause snapshot ingestion.
- `/` enters search mode. Printable characters extend the query and Backspace removes one Unicode scalar value. `Enter` leaves search-entry mode while retaining the filter. While editing, `Esc` always clears the query, leaves search mode, and keeps the TUI running, including when the query is already empty. Outside editing, `Esc` clears an accepted non-empty filter; with no filter it is a no-op.
- Matching is case-insensitive across provider display name, work summary, tmux target, and working directory. An empty query exposes every row.
- While a filter is active, `j`, `k`, `g`, `G`, and navigation operate only on visible rows. A selected incarnation remains selected when it still matches; otherwise the first matching row is selected. Clearing the filter does not resurrect a previously hidden selection.
- New snapshots are filtered immediately. The header reports visible and total rows, and a live snapshot with agents but no matches renders a distinct “No agents match this search” state.

### Connection loss

- Before the first snapshot, a failed connection renders a full error/empty state with retry guidance.
- After at least one snapshot, a disconnect retains rows in memory but marks the entire view `STALE` with the time since the last snapshot.
- The client retries with bounded exponential backoff capped at five seconds. `r` cancels the current delay and retries immediately.
- A reconnect starts from a fresh authoritative complete snapshot; equal or lower revisions are accepted because revision ordering is scoped to one stream/store lifetime. The client does not request replay from its prior version.

### Navigation and keys

| Key | Behavior |
| --- | --- |
| `j` or Down | Select next visible agent, stopping at the last row |
| `k` or Up | Select previous visible agent, stopping at the first row |
| `g` | Select first row |
| `G` | Select last row |
| `/` | Enter local incremental search mode |
| Enter | Accept a search while editing; otherwise run `tmux switch-client -c <captured-client> -t <pane-id>` for the selected row |
| Backspace | Remove the last search character while editing |
| `r` | Retry/reconnect immediately and request a fresh snapshot |
| `q` | Exit outside search mode and restore the terminal; insert `q` while editing a search |
| Esc | While editing, always clear and leave search mode; otherwise clear an accepted filter, or do nothing when no filter exists |

Navigation failure leaves the TUI running and displays the tmux error in a non-destructive status line.

### Layout

- The header contains the product name, transport connection state, monitor health, snapshot revision, counts for busy, idle, and unknown, and visible-versus-total rows when filtered. Transport `LIVE` plus degraded monitor health renders as `MONITOR DEGRADED`, not as an undifferentiated healthy state.
- A search strip makes the active query and search-entry mode explicit without relying on cursor shape.
- The primary table contains state, provider, tmux target, concise work summary, and transition age. Evidence provenance is not shown in the primary operator view.
- The selected row is identifiable without relying on color alone.
- At wide widths, a detail panel shows the selected pane's full path, pane ID, provider, complete work summary, transition time, and navigation target.
- At medium widths, hide the detail panel. At narrow widths, progressively truncate the work-summary column while preserving state, provider, and target.
- Below the minimum viable dimensions, render a resize instruction instead of a broken layout.
- An empty live snapshot renders “No configured agent panes found” and keeps connection/version information visible.

## Contract Surface

Public and cross-repository contracts are required.

### Harold API dependency

The backend plan owns a small generated `harold-api` Rust library exposing protobuf messages plus the tonic client/server types. The dashboard depends on that API library rather than copying protobuf definitions or importing Harold's application internals. Moving the dashboard into the Harold workspace later must require only path/workspace changes, not a protocol rewrite.

The dashboard consumes:

```text
WatchAgentStates(WatchAgentStatesRequest) -> stream AgentStateSnapshot
```

Required snapshot fields:

- `through_event_version`
- `server_time_ms`
- repeated monitor-health entries containing component, state, reason code, and observation time
- repeated panes containing the full agent incarnation, target, provider display name, working directory, effective state, optional `work_summary` bounded to 160 Unicode scalar values, and last transition time

An absent or empty `work_summary` renders as “No work summary reported”. Evidence provenance may remain in the transport for Harold diagnostics, but the dashboard does not expose it in its primary table or selected-agent detail.

### Command line

```text
tmx-agent-dash [--endpoint <URI>]
```

Invalid endpoints are rejected. Non-loopback endpoints are permitted only when explicitly supplied. Transport authentication beyond Harold's existing deployment boundary is outside MVP scope and must be documented as a limitation; all remote data remains untrusted display input.

Every string crossing the gRPC, tmux, endpoint, clock, or error boundary is normalized before Ratatui rendering: remove C0, C1, and ESC control sequences, preserve printable Unicode, and truncate to a documented display bound. Tmux stderr and gRPC status details are summarized from sanitized text and capped at 512 Unicode scalar values. The renderer never receives unsanitized external strings.

### Internal ports

```rust
trait AgentStateSource {
    async fn snapshots(&self) -> Result<SnapshotStream, SourceError>;
}

trait PaneNavigator {
    fn jump_to(&self, client: &str, pane_id: &str) -> Result<(), NavigationError>;
}
```

The application model and renderer depend on these ports, not directly on tonic or `std::process::Command`. Exact object-safety/generic details may follow idiomatic Rust as long as test substitution remains straightforward.

## State Model

```text
Connecting
    ├─ first snapshot ───────────────> Live(snapshot)
    └─ connection failure ──────────> Unavailable(error)

Live(snapshot)
    ├─ greater revision ────────────> Live(new snapshot)
    ├─ stream ends/fails ───────────> Stale(last snapshot, error)
    └─ quit ────────────────────────> Exiting

Unavailable(error)
    ├─ retry + snapshot ────────────> Live(snapshot)
    └─ quit ────────────────────────> Exiting

Stale(last snapshot, error)
    ├─ reconnect + full snapshot ───> Live(new snapshot)
    └─ quit ────────────────────────> Exiting
```

Terminal ownership is an outer RAII boundary around this application state machine.

## Failure Modes

| Failure mode / trigger | Expected behavior | State effect | User feedback and verification |
| --- | --- | --- | --- |
| Harold unavailable at startup | Keep retrying with capped backoff | `Unavailable` until first snapshot | Endpoint, error, and `r` hint; fake-source test |
| Stream drops after data | Retain last rows and retry | `Stale`; no local mutation of rows | Prominent stale banner and elapsed age; stream test |
| Stream is connected but one or more monitor components are degraded | Preserve the accepted snapshot and rows while separating transport liveness from monitor health | `Live` with degraded monitor status | Prominent `MONITOR DEGRADED` warning naming sanitized bounded components/reason codes; model/render/integration test |
| First reconnect snapshot has an equal or lower revision | Treat it as authoritative, mark live, and preserve selection only if the incarnation exists | Replace retained state and revision | Reconnect/store-reset regression test |
| Later duplicate snapshot revision | Ignore | No render-model change | Unit test |
| Revision regresses | Reject message and reconnect | Last valid snapshot remains stale | Protocol error status; unit test |
| Snapshot contains duplicate pane IDs or duplicate full incarnations | Reject malformed snapshot | Last valid snapshot remains | Protocol diagnostic; validation test |
| Selected pane disappears | Select nearest remaining row | Selection changes deterministically | Model test |
| Search excludes the selected pane | Select the first matching incarnation, or none when no rows match | Underlying snapshot remains unchanged | Filter/selection model test |
| Snapshot changes while filtering | Apply the new authoritative snapshot, then recompute visible rows | Search query remains client-local | Streaming filter test |
| Work summary is missing | Keep the row navigable and searchable by its other fields | None | Render “No work summary reported”; model/render test |
| No pane selected | Enter is a no-op | None | Footer guidance; key test |
| Dashboard is outside tmux | Viewing works; navigation is unavailable | None | Detail/footer explains missing tmux client; integration test |
| Pane disappears before Enter | Keep running and request/await refresh | No optimistic deletion | Tmux error status; navigator test |
| tmux switch fails | Keep selection and rows | Error status only | Exact stderr summarized; navigator test |
| Terminal is resized very small | Render resize instruction | Model retained | Snapshot/render test |
| Render, input, panic, or signal exit | Restore terminal modes | Process exits non-zero when appropriate | PTY/manual verification |
| Unknown enum value from newer server | Render `Unknown`/`None` safely | Snapshot remains usable | Forward-compatibility test |
| External value contains terminal controls or excessive text | Strip controls and truncate before it reaches the render model | Sanitized display value only | Adversarial gRPC/tmux/error tests |

## Test Theories

- **Snapshot application preserves operator context:** table-driven models cover insertion, removal, reordering, duplicate/receding revisions, empty snapshots, and selected-pane continuity.
- **Search is a pure view over live state:** table-driven models cover case-insensitive matching across all four fields, Unicode/backspace handling, no-match state, selection changes, clearing, and snapshots arriving during an active filter.
- **Agent replacement is a new row:** changed pane-root identity, matched agent PID, process start time, or provider cannot inherit selection or other client-local state from the departed incarnation.
- **Connection and monitor state never invent freshness or health:** fake streams cover startup failure, first snapshot, connected-but-monitor-degraded snapshots with retained rows, monitor recovery, disconnect, retry, equal/lower-revision reconnect, same-stream regression, and stale-age rendering.
- **Rendering receives trusted text shapes:** adversarial strings from snapshots, endpoints, tmux stderr, and transport errors are stripped and bounded before model application.
- **Navigation is one explicit effect:** fake navigators assert the captured client and selected pane; live tests cover atomic cross-session switching and disappearance races.
- **Rendering degrades by terminal width:** deterministic buffer snapshots cover wide, medium, compact, empty, unavailable, stale, and undersized layouts.
- **Terminal cleanup is unconditional and idempotent:** a PTY/subprocess harness covers partial initialization, quit, source failure, render failure, panic-hook restoration, SIGINT, SIGTERM, explicit restoration failure, exactly-once cleanup attempts, and continuation to later cleanup operations after one fails.

## Proposed Approach

```text
Harold WatchAgentStates
          │
          ▼
  tonic AgentStateSource ── snapshots/errors ─┐
                                              ▼
terminal events ───────────────────────────> App model
                                              │
                           ┌──────────────────┴─────────────────┐
                           ▼                                    ▼
                    Ratatui renderer                    PaneNavigator
                                                               │
                                                               ▼
                                                    invoking tmux client
```

Keep `App` as a functional core: snapshot validation, selection, sorting, connection state, and key decisions are pure state transitions. Tonic, terminal events, time, rendering, and tmux commands remain effectful shell adapters.

Suggested source boundaries:

```text
src/main.rs        startup, terminal guard, task orchestration
src/app.rs         pure model and update decisions
src/api.rs         Harold gRPC source adapter and snapshot mapping
src/navigation.rs  tmux client discovery and switch effect
src/ui.rs          Ratatui layout and rendering
src/text.rs        terminal-control stripping, bounds, search normalization
src/time.rs        injectable clock/age formatting if needed
```

## HTML Visual Reference

`tasks/tmux-agent-dashboard/dashboard-visual.html` is a standalone semantic HTML/CSS artifact showing:

- industrial signal-board hierarchy and palette;
- live connection/revision information;
- busy, idle, and unknown summary counts;
- selected and unselected agent rows;
- current work summaries and selected-pane details;
- an active local search with visible-versus-total results;
- disconnected/stale treatment;
- keyboard hints.

Before implementation, render it at representative wide and compact widths and use it to derive Ratatui color, border, spacing, and visibility decisions. Do not attempt pixel fidelity in terminal cells.

## Acceptance Criteria

- Starting the dashboard against Harold shows the complete stored projection before later updates.
- Busy, idle, and unknown agents are visually distinct without color being the only signal.
- Each row answers what the agent is working on, or explicitly says that no work summary was reported; evidence provenance does not occupy primary UI space.
- `/` incrementally filters the live snapshot by provider, work summary, target, and directory without interrupting streaming updates.
- Filtered selection and no-match behavior remain deterministic as snapshots change.
- Snapshot revisions update rows without losing a still-valid selection.
- A connected stream with degraded monitor health preserves pane rows but cannot render as fully healthy; monitor recovery clears the warning only after an accepted healthy snapshot.
- Disconnect retains but clearly marks stale state; reconnect replaces it with Harold's fresh complete snapshot.
- An equal- or lower-revision first snapshot on a reconnected stream becomes authoritative and clears stale status.
- Enter navigates the invoking tmux client atomically to the selected pane.
- Viewing works outside tmux and explains why navigation is disabled.
- All supported exit paths attempt each required terminal cleanup operation exactly once, including partial initialization failure, panic, render failure, SIGINT, SIGTERM, and explicit restoration failure. A failed cleanup is reported, later cleanup operations are still attempted, and every operation that can succeed restores its mode.
- No external string can inject terminal controls or grow beyond its documented display bound.
- The wide, compact, empty, unavailable, connected-but-monitor-degraded, stale, and undersized layouts are verified.
- The rendered TUI follows the approved HTML reference's information hierarchy.
- No dependency is added or fetched before the required Rust supply-chain audit.

## Verification Plan

- Follow red-green-refactor for the app model, stream adapter, navigation adapter, and layouts.
- Run `cargo fmt --all -- --check`.
- Run `cargo test --all-targets`.
- Run `cargo clippy --all-targets --all-features -- -D warnings`.
- Run `cargo build --release`.
- Run deterministic Ratatui `TestBackend` snapshots for every specified layout/state.
- Run PTY/subprocess proofs against a temporary Harold instance and live tmux server for partial terminal-initialization failures, panic, render failure, SIGINT, SIGTERM, explicit restoration failure, and exactly-once cleanup.
- Render and inspect the HTML visual at wide and compact widths before implementing the TUI.
- Obtain the required independent completion review and resolve every finding.

## Documentation Notes

After implementation, publish:

- a how-to for installation, endpoint selection, and running inside tmux;
- a key, search, and state reference;
- troubleshooting for Harold connection, stale views, and navigation failures.

Do not migrate mock data, speculative module names, or browser-only visual decoration from the HTML artifact.

## Risks And Approved Decisions

- The user approved Harold as the inventory, screen-evidence, durable-event, projection, and live-stream authority.
- The user approved snapshot-first reconnection and deferred cursor-based event replay.
- The user requested screen scraping as a reliability signal; the dashboard consumes only Harold's classified evidence and never receives raw pane contents.
- The user approved durable bounded work summaries, with explicit agent input preferred and visible-screen extraction only as a fallback; the dashboard displays the summary rather than evidence provenance.
- The user approved dashboard-local incremental search over the current streamed snapshot; Harold does not gain a search API for the MVP.
- The eventual move into Harold is preserved by the small shared API boundary and the dashboard's lack of backend responsibilities.
