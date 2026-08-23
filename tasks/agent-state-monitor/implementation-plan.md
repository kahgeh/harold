# Agent State Monitor Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Harold durably discover and monitor tmux agents, publish snapshot-first state with one concise current-work summary per pane, and keep raw screen content and dashboard search outside Harold's persistence and API boundaries.

**Architecture:** A single `AgentMonitorRuntime` serializes inventory, lifecycle, completion, screen, departure, and candidate-repair decisions into Harold's existing ordered `EventStream`. An application projector atomically reduces those events into TursoDB current state while staging only notification/routing events into the delivery outbox; after commit, a seeded `watch` publisher feeds a shared gRPC snapshot contract. Durable candidate timestamps select the latest substantive explicit or screen summary, with explicit winning ties, and only sanitized bounded summary candidates cross the acquisition boundary.

**Tech Stack:** Rust 2024, Tokio, Tonic/Prost, the workspace `events` crate, TursoDB via `turso = 0.5.1`, Serde JSON events, tmux and macOS `ps` command adapters.

**Spec:** `tasks/agent-state-monitor/spec.md`

## Global Constraints

- Do not implement dashboard search in Harold; no search RPC, request field, index, or stored query is permitted.
- `TurnCompleteRequest` fields 1 through 5 remain wire-compatible; normalized-empty legacy `last_user_prompt = 3` maps to `Unchanged`, an exact configured placeholder clears the serialized raw prompt and also maps to `Unchanged`, other non-empty input maps to `Set`, and this legacy input can never clear.
- `ReportAgentStateRequest.optional work_summary = 4`: absent is `Unchanged`, present-normalized-empty is `Clear`, exact-configured-placeholder is `Unchanged`, and other present-normalized-non-empty input is `Set`.
- `AgentPaneState.optional work_summary = 14` is the entire public summary contract; absence means no usable summary and dashboard copy is exactly `No work summary reported`. Do not add a public summary timestamp without a demonstrated consumer requirement.
- Work summaries are normalized before append: strip C0, C1, and ESC sequences, collapse Unicode whitespace, trim, then truncate to 160 Unicode scalar values.
- Raw `tmux capture-pane` output must never enter an event, TursoDB row, API message, diagnostic, error, tracing field, or test failure snapshot.
- The most recently observed substantive explicit or screen candidate is effective, with explicit winning equal timestamps; new incarnations inherit neither.
- Every pane-scoped event carries `(pane_id, pane_pid, agent_pid, agent_started_at_ms, provider_id)`.
- Agent-only events update the application projection but never enter the external-delivery outbox.
- Additive checksum-tracked migrations only; do not edit migrations already applied by `HaroldStore`.
- Do not edit a Cargo manifest or run a dependency-fetching command until the required Rust supply-chain auditor approves every new direct dependency and version.
- Preserve unrelated and concurrent work; inspect `git status --short` before every commit and stage explicit paths only.
- Do not deploy during implementation unless the coordinator separately authorizes deployment.

## Exact Shared Protobuf Contract

Move the canonical schema to `harold-api/proto/harold.proto`. Preserve `TurnCompleteRequest` fields 1-5 and add exactly:

```proto
service Harold {
  rpc TurnComplete(TurnCompleteRequest) returns (TurnCompleteResponse);
  rpc ReportAgentState(ReportAgentStateRequest) returns (ReportAgentStateResponse);
  rpc WatchAgentStates(WatchAgentStatesRequest) returns (stream AgentStateSnapshot);
}

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

message ReportAgentStateRequest {
  string pane_id = 1;
  AgentState state = 2;
  string adapter_id = 3;
  optional string work_summary = 4;
}

message ReportAgentStateResponse { bool accepted = 1; }
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

`work_summary` is the only summary field exposed to dashboard consumers. Do not publish a summary timestamp, internal summary candidates, adapter/classifier IDs, evidence provenance, or raw/cropped screen content.

## File Map

- Create `harold-api/`: canonical protobuf, generated shared client/server/types, and wire-contract tests.
- Create `harold/src/agent/{mod,domain,summary,inventory,screen,reducer,runtime,snapshot}.rs` with sibling focused test modules.
- Create `harold/src/store/migrations/003_agent_monitor_projection.sql` for `agent_panes` and `agent_monitor_health`.
- Modify `harold/src/store.rs` for domain appends, atomic projection, snapshot reads, and migration registration.
- Modify `harold/src/projector.rs` to separate projection from external delivery and publish only committed snapshots.
- Modify `harold/src/main.rs` for shared API imports, RPCs, startup catch-up, runtime lifecycle, and graceful shutdown.
- Modify `harold/src/settings.rs`, configs, and `harold/src/inbound/tmux.rs` for named providers and legacy compatibility.
- Modify `hooks/shared/harold_turn_complete.py` only to preserve the existing payload and defense-in-depth cleaning.
- Modify `Makefile` to deploy the canonical shared proto.
- Delete `harold/build.rs` and `harold/proto/harold.proto` only after the shared crate is green.

---

### Task 1: Baseline and dependency-audit gate

**Files:** Inspect `Cargo.toml`, `Cargo.lock`, `harold/Cargo.toml`, `harold/build.rs`, and `harold/proto/harold.proto`; modify only `tasks/agent-state-monitor/todo.md` to record results.

**Interfaces:** Produces approval or rejection for direct `tokio-stream = 0.1.18`; confirms reuse of existing Tonic/Prost versions.

- [x] **Step 1: Capture the no-fetch baseline**

```bash
git status --short
cargo test --offline -p harold --all-targets
cargo test --offline -p events --all-targets
cargo audit --no-fetch
```

Expected: existing tests pass; only the already documented accepted transitive `paste 1.0.15` warning may remain.

- [x] **Step 2: Run the required auditor before any manifest edit**

Coordinator-provided independent Rust audit approved exactly `tokio-stream = { version = "=0.1.18", default-features = false }`, checksum `32da49809aab5c3bc678af03902d4ccddea2a87d028d86392a4b1560c6906c70`, using `tokio_stream::wrappers::ReceiverStream`. It is already locked and cached, adds no package/version, and the local `harold-api` crate reuses `tonic 0.14.5`, `prost 0.14.3`, and `tonic-prost-build 0.14.5`.

- [x] **Step 3: Record the gate**

Update the task todo with baseline and auditor outcomes. Do not change production files.

Execution record: `cargo test --offline -p harold --all-targets` passed 37 tests; `cargo test --offline -p events --all-targets` passed 52 tests; `cargo audit --no-fetch` exited 0 with only allowed `RUSTSEC-2024-0436` for transitive `paste 1.0.15` through Turso.

### Task 2: Shared protobuf crate and compatibility

**Files:** Create `harold-api/{Cargo.toml,build.rs,proto/harold.proto,src/lib.rs,tests/contract.rs}`; modify root/Harold manifests, `harold/src/main.rs`, `harold/src/main_tests.rs`, and `Makefile`; delete old Harold build/schema after green.

**Interfaces:** Produces `harold_api::harold::{harold_client, harold_server, AgentPaneState, AgentStateSnapshot, ReportAgentStateRequest, TurnCompleteRequest, WatchAgentStatesRequest}`.

- [x] **Step 1: Write failing contract tests**

In `contract.rs`, Prost-round-trip `ReportAgentStateRequest` with `work_summary: None`, `Some("")`, and `Some("task")`; assert presence survives. Round-trip `AgentPaneState.work_summary` as absent and present, assert no public `work_summary_updated_at_ms` field exists, and round-trip all existing `TurnCompleteRequest` fields.

- [x] **Step 2: Observe red**

Run `cargo test --offline -p harold-api --test contract`.

Expected: FAIL because the workspace member does not exist.

Execution record: exited 101 with `package ID specification 'harold-api' did not match any packages`.

- [x] **Step 3: Add minimal shared generation**

Use `tonic_prost_build::configure()` to compile `proto/harold.proto` and emit a descriptor set used only by schema contract tests. Expose generated types under `pub mod harold { tonic::include_proto!("harold"); }` and expose the descriptor bytes for those tests. Add the workspace member/path dependency and remove Harold's code-generation build dependency after imports compile. Keep the not-yet-implemented RPC methods as explicit `UNIMPLEMENTED` stubs using the audited `tokio_stream::wrappers::ReceiverStream` associated stream type, with a focused Harold test proving that staged behavior.

- [x] **Step 4: Prove green and no search surface**

```bash
cargo test --offline -p harold-api --test contract
cargo check --offline -p harold --all-targets
! rg -n 'rpc (Search|Query)|search_term|query' harold-api/proto/harold.proto
```

Execution record: four `harold-api` contract tests passed; the focused staged-RPC test first failed with generated-trait error E0046 before the stubs existed, then passed after the minimal stubs were restored; `cargo check --offline -p harold --all-targets` passed; the schema scan found no search/query, evidence/provenance, or summary-timestamp surface.

- [x] **Step 5: Switch Makefile to `harold-api/proto/harold.proto`, remove old generator/schema, verify, review, and commit explicit paths**

Commit message: `feat: share agent monitor protobuf contract`.

Execution record: fresh format, focused API/Harold/events tests, Harold/API Clippy, Harold check, no-fetch audit, forbidden-surface scan, registry-package-count comparison, deploy dry-run, and diff checks passed. The independent completion reviewer found no issues and approved this checkpoint. Only the 13 approved production/schema paths were staged; commit `7e226b7` created the shared-contract checkpoint. Task 3 remains gated.

### Task 3: Summary normalization and typed presence semantics

**Files:** Create `harold/src/agent/{mod,domain,summary,summary_tests}.rs`; modify `harold/src/main.rs` to register the module only.

**Interfaces:**

```rust
pub(crate) const WORK_SUMMARY_MAX_SCALARS: usize = 160;

pub(crate) enum WorkSummaryUpdate {
    Unchanged,
    Clear,
    Set(String),
}

impl Default for WorkSummaryUpdate {
    fn default() -> Self { Self::Unchanged }
}

pub(crate) enum CompletionSummaryUpdate {
    Unchanged,
    Set(String),
}

impl Default for CompletionSummaryUpdate {
    fn default() -> Self { Self::Unchanged }
}

pub(crate) enum ObservedAgentState {
    Busy,
    Idle,
}

pub(crate) enum EffectiveAgentState {
    Busy,
    Idle,
    Unknown,
}

pub(crate) fn normalize_work_summary(input: &str) -> Option<String>;
pub(crate) fn explicit_summary_update(input: Option<&str>) -> WorkSummaryUpdate;
pub(crate) fn completion_summary_update(input: &str) -> CompletionSummaryUpdate;
```

- [x] **Step 1: Write failing table tests**

Cover ASCII, printable Unicode, CSI/OSC/ESC, C0/C1, Unicode whitespace, empty normalized input, and 160/161-scalar boundaries. Failure messages must not interpolate raw input.

- [x] **Step 2: Test the distinct RPC and legacy completion conversions**

Assert RPC conversion `None -> Unchanged`, normalized empty `Some` -> `Clear`, and non-empty `Some` -> bounded `Set`. Separately assert legacy completion conversion normalized empty -> `CompletionSummaryUpdate::Unchanged` and non-empty -> `CompletionSummaryUpdate::Set`; the completion type has no `Clear` variant.

- [x] **Step 3: Observe red with `cargo test --offline -p harold agent::summary`**

Execution record: RED failed with E0583 because the wished-for `agent::domain` and `agent::summary` modules did not exist. The first GREEN attempt exposed an invalid CSI fixture (`u` is a legal CSI final byte); the fixture was corrected to a genuinely unterminated `ESC [ 31;` sequence before changing production behavior.

- [x] **Step 4: Implement one single-pass trust-boundary normalizer**

Do not reuse outbound `inbound::tmux::strip_control`; it does not cover the full ingress requirement.

- [x] **Step 5: Run green**

```bash
cargo test --offline -p harold agent::summary
cargo clippy --offline -p harold --all-targets -- -D warnings
```

Execution record: five focused summary tests passed and Harold all-target Clippy passed with warnings denied. The normalizer preserves printable Unicode, strips terminal control sequences, collapses Unicode whitespace, and caps output at 160 Unicode scalar values; legacy normalized-empty remains `Unchanged` while only present optional input can produce `Clear`.

- [x] **Step 6: Commit `feat: normalize bounded agent work summaries`**

Execution record: after fresh focused tests, format, Clippy, and cached-diff inspection, only the five Task 3 code paths were staged and committed as `fd0b93e`.

### Task 4: Named settings and incarnation-safe inventory

**Files:** Create `harold/src/agent/inventory.rs` and `harold/src/agent/inventory_tests.rs`; modify `harold/src/agent/domain.rs`, `harold/src/settings.rs`, `harold/src/inbound/tmux.rs`, `harold/src/inbound/tmux_tests.rs`, `harold/config/default.toml`, and `harold/config/local.template.toml`.

**Interfaces:**

```rust
pub(crate) struct AgentIncarnation {
    pub pane_id: String,
    pub pane_pid: u32,
    pub agent_pid: u32,
    pub agent_started_at_ms: i64,
    pub provider_id: String,
}

pub(crate) struct AgentPaneObservation {
    pub incarnation: AgentIncarnation,
    pub tmux_target: String,
    pub session_name: String,
    pub window_index: u32,
    pub pane_index: u32,
    pub working_directory: String,
    pub provider_display_name: String,
    pub observed_at_ms: i64,
}

pub(crate) struct AgentProviderSettings {
    pub id: String,
    pub display_name: String,
    pub command_contains: Vec<String>,
    pub busy_all: Vec<String>,
    pub idle_all: Vec<String>,
    pub summary_line_prefixes: Vec<String>,
}

pub(crate) struct AgentMonitorSettings {
    pub inventory_interval_ms: u64,
    pub screen_interval_ms: u64,
    pub hook_grace_ms: u64,
}

pub(crate) enum InventoryError {
    CommandUnavailable,
    CommandFailed,
    MalformedOutput,
    MissingProcessStartTime,
}

pub(crate) trait AgentInventoryPort: Send + Sync {
    fn scan(&self) -> Result<Vec<AgentPaneObservation>, InventoryError>;
    fn resolve(&self, pane_id: &str) -> Result<Option<AgentPaneObservation>, InventoryError>;
    fn is_current(&self, incarnation: &AgentIncarnation) -> Result<bool, InventoryError>;
}
```

- [x] **Step 1: Write failing config tests** for named providers, legacy flat matcher, invalid/duplicate IDs, empty fragments, and zero intervals.

- [x] **Step 2: Write failing pure process tests** for foreground preference, shallowest descendant, PID tie-break, wrappers, ambiguity, missing start time, provider replacement, and PID reuse with a new start time.

- [x] **Step 3: Observe red**

```bash
cargo test --offline -p harold settings
cargo test --offline -p harold agent::inventory
```

Execution record: the settings RED failed on missing migration-aware `AgentMonitorSettings`/`AgentProviderSettings` and absent `Named`/`Legacy` variants. The inventory RED failed on missing `agent::inventory`, `AgentIncarnation`, and unknown-provider identity. A separate inbound bridge RED failed because `observation_to_address` did not exist.

- [x] **Step 4: Implement parsing/selection without a new dependency**

Extend `ps` input with process group, TTY, command, and start time. Preserve inbound routing by mapping observations to existing `AgentAddress`.

- [x] **Step 5: Run green/regression tests**

```bash
cargo test --offline -p harold agent::inventory
cargo test --offline -p harold inbound::tmux
cargo test --offline -p harold inbound
```

Execution record: five settings tests, nine inventory tests, three tmux bridge/control tests, and all 12 inbound tests passed; Harold all-target Clippy passed with warnings denied. Named and deprecated legacy config shapes load, old local tables replace named defaults, `ps` start times use the existing `time` dependency, and no Cargo file or dependency changed.

- [x] **Step 6: Commit `feat: identify named agent incarnations`**

Execution record: after fresh focused regressions, format, Clippy, and cached-diff inspection, only the nine Task 4 code/config paths were staged and committed as `4b27040`.

### Task 5: Visible-screen state and fallback-summary acquisition

**Files:** Create `harold/src/agent/screen.rs` and `harold/src/agent/screen_tests.rs`; modify `harold/src/agent/domain.rs` and `harold/src/agent/mod.rs`.

**Interfaces:**

```rust
pub(crate) struct ScreenObservation {
    pub incarnation: AgentIncarnation,
    pub state: Option<ObservedAgentState>,
    pub fallback_summary: Option<String>,
    pub classifier_id: String,
    pub observed_at_ms: i64,
}

pub(crate) enum ScreenError {
    CaptureUnavailable,
    CaptureFailed,
    PaneDeparted,
}

pub(crate) trait VisibleScreenPort: Send + Sync {
    fn observe(
        &self,
        pane: &AgentPaneObservation,
        provider: &AgentProviderSettings,
    ) -> Result<ScreenObservation, ScreenError>;
}
```

- [x] **Step 1: Write failing classifier/extractor fixtures**

Cover busy, idle, both markers (busy wins), state-only, summary-only with inconclusive state, both present, both inconclusive, ANSI/Unicode, cropped content, bottom-most configured prefix, empty suffix, and a secret elsewhere in the screen. Assert state and fallback summary are independent optional outputs.

- [x] **Step 2: Write a failing capture-command boundary test**

Require current grid only (`capture-pane -p -t <pane>` or `-S 0`), never negative history. Errors contain bounded reason/status only, not stdout/stderr or capture text.

- [x] **Step 3: Observe red with `cargo test --offline -p harold agent::screen`**

Execution record: RED failed with E0583 because the wished-for `agent::screen` module did not exist.

- [x] **Step 4: Implement provider-local acquisition**

Keep the raw string local to `observe`; pass a conclusive extracted candidate through `normalize_work_summary`, construct `ScreenObservation` with independent `state: Option<ObservedAgentState>` and `fallback_summary: Option<String>`, and drop raw content before return. Do not derive `Debug` on a raw-capture holder. The runtime compares each `Some` with its last successfully appended conclusive value, converts unchanged values to `None`, and appends durable `AgentScreenObserved` if either changed value remains. This produces honest summary-only and state-only events; both absent after deduplication is a no-op. No screen fact is published directly.

- [x] **Step 5: Prove green and manually inspect leakage points**

```bash
cargo test --offline -p harold agent::screen
rg -n 'capture|screen|stdout|stderr' harold/src/agent/screen.rs
```

Expected: no captured variable is formatted, logged, or returned.

Execution record: six screen tests and all five summary normalization regressions passed; Harold all-target Clippy passed with warnings denied. Manual source inspection found raw stdout only in a non-`Debug` private command-output holder and the local `captured` value passed directly into typed classification/extraction; no tracing, formatting, error payload, or return path contains raw screen text. Deduplication/event append remains correctly deferred to Task 6/runtime.

Independent review RED/GREEN record: review found that raw process commands were still reachable through derived `Debug`, terminal control-string coverage omitted DCS/SOS/PM/APC and C1 string termination, named provider ambiguity had no bounded warning and could collide with a configured `unknown` id, and Claude/OpenCode had no verified screen clauses. Each fix began with a failing test: the command sentinel appeared in `ProcessInfo` debug output; new C1/control-string fixtures leaked or truncated printable suffixes; `unknown` passed settings validation; typed ambiguous resolution did not compile; and default-provider contract assertions failed on Claude's empty clauses. GREEN now uses a manually redacted `ProcessInfo` formatter, strips OSC/DCS/SOS/PM/APC for ESC and C1 forms, reserves `unknown`, returns typed ambiguity and logs only `provider_match_count`, and tests current default screen contracts. Claude uses `esc to interrupt`, `❯`, and `❯`; OpenCode 1.4.10 uses `esc interrupt` and conjunctive `agents`/`commands`. OpenCode deliberately has no fallback-summary prefix because its official TUI source gives the live prompt and rendered user messages the same `┃` prefix; configuring it would persist the idle `Ask anything...` placeholder as work.

Post-fix focused verification: summary 5/5, settings 6/6, inventory 11/11, tmux bridge 3/3, inbound 12/12, screen 6/6, `harold-api` contract 4/4, Harold all-targets 63/63, all-target Harold Clippy with warnings denied, format, and diff checks passed offline. No Cargo file or dependency changed.

- [x] **Step 6: Commit `feat: acquire agent state and summary fallback`**

Execution record: after the approved independent Rust review, a fresh offline run repeated all focused suites, API contract tests, Harold 63/63, format, Clippy, forbidden public-surface assertions, no-Cargo-diff gate, and diff checks. Exactly the ten Task 5/review-fix production paths were staged, the cached diff was inspected, and commit `22a24aa` was created. Task documentation and concurrent work remained unstaged.

### Task 6: Durable agent events and pure reconciliation

**Files:** Create `harold/src/agent/reducer.rs` and `harold/src/agent/reducer_tests.rs`; modify `harold/src/agent/domain.rs`, `harold/src/store.rs`, and `harold/src/store_tests.rs`.

**Interfaces:**

```rust
pub(crate) enum AgentEvent {
    PaneObserved(AgentPaneObserved),
    PaneDeparted(AgentPaneDeparted),
    LifecycleObserved(AgentLifecycleObserved),
    ScreenObserved(AgentScreenObserved),
    MonitorHealthChanged(AgentMonitorHealthChanged),
}

pub(crate) struct AgentPaneObserved {
    pub pane: AgentPaneObservation,
}

pub(crate) struct AgentPaneDeparted {
    pub incarnation: AgentIncarnation,
    pub observed_at_ms: i64,
}

pub(crate) struct AgentLifecycleObserved {
    pub incarnation: AgentIncarnation,
    pub state: ObservedAgentState,
    pub adapter_id: String,
    pub work_summary: WorkSummaryUpdate,
    pub observed_at_ms: i64,
}

pub(crate) struct AgentScreenObserved {
    pub incarnation: AgentIncarnation,
    pub state: Option<ObservedAgentState>,
    pub classifier_id: String,
    pub fallback_summary: Option<String>,
    pub observed_at_ms: i64,
}

pub(crate) struct AgentMonitorHealthChanged {
    pub component: String,
    pub healthy: bool,
    pub reason_code: String,
    pub observed_at_ms: i64,
}

pub(crate) struct AgentPaneProjection {
    pub pane: AgentPaneObservation,
    pub hook_state: Option<ObservedAgentState>,
    pub hook_observed_at_ms: Option<i64>,
    pub screen_state: Option<ObservedAgentState>,
    pub screen_classifier_id: Option<String>,
    pub screen_observed_at_ms: Option<i64>,
    pub effective_state: EffectiveAgentState,
    pub explicit_work_summary: Option<String>,
    pub explicit_work_summary_updated_at_ms: Option<i64>,
    pub screen_work_summary: Option<String>,
    pub screen_work_summary_updated_at_ms: Option<i64>,
    pub work_summary: Option<String>,
    pub last_transition_at_ms: i64,
    pub last_event_version: EventStreamVersion,
}

pub(crate) struct MonitorHealthProjection {
    pub component: String,
    pub healthy: bool,
    pub reason_code: String,
    pub observed_at_ms: i64,
    pub last_event_version: EventStreamVersion,
}

pub(crate) enum ProjectionChange {
    Upsert(AgentPaneProjection),
    Remove(AgentIncarnation),
    Ignore,
}

pub(crate) struct AgentSnapshot {
    pub through_event_version: EventStreamVersion,
    pub server_time_ms: i64,
    pub monitor_health: Vec<MonitorHealthProjection>,
    pub panes: Vec<AgentPaneProjection>,
}

pub(crate) fn reduce_agent_event(
    current: Option<AgentPaneProjection>,
    event: &AgentEvent,
    event_version: EventStreamVersion,
    hook_grace_ms: u64,
) -> ProjectionChange;

pub(crate) async fn append_agent_events(
    store: &HaroldStore,
    events: Vec<AgentEvent>,
) -> events::Result<events::AppendResult>;
```

- [x] **Step 1: Write failing real-stream serialization tests**

Append/reload every variant and assert full incarnation, typed summary update, normalized-only summary content, and ordered versions. For `AgentScreenObserved`, round-trip state-only, summary-only, and both-present payloads; reject/no-op both-absent before append. A pane+lifecycle pair must be one contiguous append result.

- [x] **Step 2: Write failing reducer tables**

Cover unknown startup, hook grace, screen repair, explicit set/unchanged/clear, fallback selection/inconclusive, old-incarnation ignore, replacement clearing, duplicate observations, and matching/non-matching departure. Include `AgentScreenObserved` cases for state-only, summary-only, both present, and both absent; each present fact updates independently and absence preserves its candidate. `Unchanged` preserves the explicit summary, `Set` replaces it, and clearing without fallback makes effective summary absent. A screen state observed during grace remains subordinate to the hook; one meaningful identical-state post-grace revalidation may refresh its observation and repair the hook, while another identical event after that state is already effective does not refresh observation or transition metadata. Repeated identical screen-summary candidates remain deduplicated. Transition time changes only with effective state.

- [x] **Step 3: Observe red**

```bash
cargo test --offline -p harold agent::reducer
cargo test --offline -p harold store::tests
```

Execution record: both commands exited 101 on the intended absent Task 6 contracts (`agent::reducer`, durable event payloads/projections, and `append_agent_events`; E0432/E0425).

- [x] **Step 4: Implement stable event strings and pure reduction**

Persist internal evidence/candidates and their observation timestamps for deterministic replay; the public projection mapping exposes only effective `work_summary: Option<String>`.

- [x] **Step 5: Run green**

```bash
cargo test --offline -p harold agent::reducer
cargo test --offline -p harold store
```

Execution record: reducer 8/8, store 8/8, Harold all-targets 74/74, format, all-target Harold Clippy with warnings denied, no-Cargo-diff, and diff checks passed. The reducer takes explicit `hook_grace_ms` to remain pure and configurable.

- [x] **Step 6: Commit `feat: persist agent monitor observations`**

Execution record: `072e7fb` created the six-path Task 6 slice. Independent Rust review found a hook-grace/screen-dedupe contradiction and incomplete incarnation assertions. Fix `4e22eae` scopes meaningful identical-state repair to a lifecycle reconciliation epoch and makes the reducer ignore later already-effective duplicates; fix `afb0126` restores complete pane payload round-trip coverage while retaining explicit five-field incarnation assertions. Fresh focused/full tests and Clippy passed, and the independent scoped re-review approved Task 6 with no remaining findings.

### Task 7: Atomic TursoDB projection and selective outbox

**Files:** Create `harold/src/store/migrations/003_agent_monitor_projection.sql`; modify `harold/src/store.rs`, `harold/src/store_tests.rs`, `harold/src/projector.rs`, and `harold/src/projector_tests.rs`.

**Interfaces:** Replace `stage_unhandled_events` with:

```rust
pub(crate) struct ProjectionBatch {
    pub applied: usize,
    pub through_event_version: EventStreamVersion,
    pub snapshot_changed: bool,
}

pub(crate) async fn project_unhandled_events(
    &self,
    limit: usize,
) -> events::Result<ProjectionBatch>;

pub(crate) async fn load_agent_snapshot(&self) -> events::Result<AgentSnapshot>;
```

Migration 003 creates exact nullable/current-state columns matching `AgentPaneProjection`: identity and display metadata, hook/screen evidence timestamps, `effective_state`, `explicit_work_summary`, `explicit_work_summary_updated_at_ms`, `screen_work_summary`, `screen_work_summary_updated_at_ms`, `work_summary`, `last_transition_at_ms`, and `last_event_version`; primary key is `pane_id`. Add presence checks for each internal candidate/timestamp pair and `CHECK(length(work_summary) <= 160)`. There is no effective/public summary timestamp column. `agent_monitor_health` uses `component` as primary key and stores `healthy`, bounded `reason_code`, `observed_at_ms`, and `last_event_version`.

- [x] **Step 1: Write failing migration tests**

Assert migration 003 is idempotent, applied-checksum changes fail, nullable summaries round-trip, and earlier migration records remain unchanged.

- [x] **Step 2: Write failing atomic mixed-event tests**

Mix `TurnCompleted`, `InboundMessageReceived`, and agent events. One transaction must project agent state, stage only the first two event types, and advance checkpoint. Inject failure before checkpoint/commit; assert no partial row/outbox/checkpoint and replay convergence after reopen.

- [x] **Step 3: Write failing summary durability/privacy tests**

Project explicit, summary-only screen fallback, state-only screen observation, clear, and replacement; reopen and assert the same effective summary/version and internal candidate observation times. Exercise internal candidate nullability checks. Search event payloads and projection text columns for a raw-screen sentinel and require zero matches.

- [x] **Step 4: Observe red**

```bash
cargo test --offline -p harold store::tests
cargo test --offline -p harold projector::tests
```

- [x] **Step 5: Implement additive SQL and one transaction**

Register SQL with `include_str!`. Apply agent events to projection, deliverable events to outbox, and treat unknown event types as poison without silently advancing. Publish nothing inside the transaction.

- [x] **Step 6: Run green and no-fetch audit**

```bash
cargo test --offline -p harold store::tests
cargo test --offline -p harold projector::tests
cargo audit --no-fetch
```

- [x] **Step 7: Commit `feat: project current agent state atomically`**

Execution record: RED exited 101 on the absent projection/snapshot interface. GREEN added checksum-tracked migration 003, one atomic projection/outbox/checkpoint transaction, restart/rollback recovery, summary-candidate constraints, and raw-screen sentinel coverage. Independent review found hard-coded grace and a read-only writer lock; `bd883bf` made configured grace store-owned and replaced the snapshot transaction with one typed SQLite statement. Final store 14/14, projector 6/6, Harold 81/81, Clippy/audit/diff gates passed and independent re-review approved commits `f154843` and `bd883bf`.

### Task 8: Serialized monitor runtime and completion integration

**Files:** Create `harold/src/agent/runtime.rs` and `harold/src/agent/runtime_tests.rs`; modify `harold/src/agent/mod.rs`, `harold/src/store.rs`, `harold/src/main.rs`, and `harold/src/main_tests.rs`.

**Interfaces:**

```rust
pub(crate) enum AgentMonitorCommand {
    ReportLifecycle {
        pane_id: String,
        state: ObservedAgentState,
        adapter_id: String,
        work_summary: WorkSummaryUpdate,
        reply: tokio::sync::oneshot::Sender<Result<(), MonitorCommandError>>,
    },
    TurnCompleted {
        turn: TurnCompleted,
        reply: tokio::sync::oneshot::Sender<events::Result<events::AppendResult>>,
    },
    InventoryTick,
    ScreenTick,
}

pub(crate) struct AgentMonitorHandle {
    sender: tokio::sync::mpsc::Sender<AgentMonitorCommand>,
}

pub(crate) enum MonitorCommandError {
    InvalidInput,
    AgentNotFound,
    InventoryUnavailable,
    EventAppend(events::EsError),
    RuntimeStopped,
}

// Extend the existing persisted type; serde defaults keep older events readable.
pub(crate) struct TurnCompleted {
    pub pane_id: String,
    pub pane_label: String,
    pub last_user_prompt: String,
    pub assistant_message: String,
    pub main_context: String,
    #[serde(default)]
    pub agent_incarnation: Option<AgentIncarnation>,
    #[serde(default)]
    pub work_summary: CompletionSummaryUpdate,
}
```

- [x] **Step 1: Write failing lifecycle tests**

Fake inventory + real store: valid report appends pane then lifecycle contiguously; unresolved is a typed precondition error; invalid identifiers/state reject before send; append failure returns and does not advance dedupe.

- [x] **Step 2: Write failing completion tests**

Use `completion_summary_update(last_user_prompt)`: normalized empty produces `CompletionSummaryUpdate::Unchanged`, while non-empty produces bounded `CompletionSummaryUpdate::Set`; `Clear` is not representable. Retain existing notification fields at compatibility bounds. Resolved completion appends pane then `TurnCompleted` with incarnation/derived summary; unresolved completion still appends one notification event and succeeds.

- [x] **Step 3: Write failing race, departure, and screen-delta tests**

Cover lifecycle/departure orders, two-successful-scan confirmation, failed scan retention, and exact-incarnation revalidation. Also feed state-only, summary-only, both-changed, unchanged, and both-inconclusive screen results; assert the runtime appends only successfully changed present facts and advances each dedupe value only after append success. Lifecycle acceptance starts a new screen-state reconciliation epoch for that incarnation: conflicting state during hook grace is retained pending without advancing state dedupe, then appended once after grace if still visible; further identical samples are suppressed. Independent summary changes may still append during grace. Successful race orders converge on actual liveness.

- [x] **Step 4: Observe red with `cargo test --offline -p harold agent::runtime`**

- [x] **Step 5: Implement one bounded command loop**

Only the runtime calls `append_agent_events`; update dedupe only from successful append results. Separate Tokio intervals enqueue ticks so decisions serialize.

- [x] **Step 6: Route `TurnComplete` through the runtime**

Keep payload/response compatibility. Log bounded identifiers and result codes only; do not log request content.

- [x] **Step 7: Run green/regression tests**

```bash
cargo test --offline -p harold agent::runtime
cargo test --offline -p harold main::tests
cargo test --offline -p harold projector::tests
```

- [x] **Step 8: Commit `feat: serialize agent monitor lifecycle events`**

Execution record: initial RED exited 101 on the absent runtime and completion fields. Subsequent REDs covered append-failure dedupe, departure failure retention, completion-idle projection, grace-epoch agreement, restart seeding, permanent acquisition wedges, health transitions, metadata bounds/privacy, poll starvation, same-state restart revalidation, metadata normalization equivalence, and zero-grace equal-millisecond ordering. GREEN uses serialized prioritized ingress, capacity-one coalesced ticks, one detached bounded acquisition worker per port, durable health, full-incarnation restart state, and explicit lifecycle invalidation of the durable screen-state epoch while preserving fallback summaries. Independent review approved `1725f2e`, `1715755`, `2cc6682`, and `b81438c`; final focused/full GREEN was Harold 109/109 with Clippy/audit/no-Cargo/diff gates clean.

### Task 9: Stored snapshot publisher and snapshot-first stream

**Files:** Create `harold/src/agent/snapshot.rs` and `harold/src/agent/snapshot_tests.rs`; modify `harold/src/agent/mod.rs`, `harold/src/projector.rs`, `harold/src/projector_tests.rs`, `harold/src/main.rs`, and `harold/src/main_tests.rs`.

**Interfaces:**

```rust
pub(crate) struct AgentSnapshotHub {
    sender: tokio::sync::watch::Sender<AgentSnapshot>,
    _receiver: tokio::sync::watch::Receiver<AgentSnapshot>,
}

impl AgentSnapshotHub {
    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<AgentSnapshot>;
    pub(crate) fn publish_committed(&self, snapshot: AgentSnapshot);
}
```

- [x] **Step 1: Write failing hub tests**

Assert DB-seeded initial value, zero-subscriber retention, increasing revisions, slow-receiver coalescing, and no publication after rollback.

- [x] **Step 2: Write failing watch RPC tests**

Connect before/after changes; first message is complete stored state, later revision is greater, reconnect sees current truth, `work_summary` maps as `Some`/`None`, and public types have no summary timestamp, provenance, or candidate fields. Open a stream, trigger shutdown, and require closure within one second.

- [x] **Step 3: Observe red**

```bash
cargo test --offline -p harold agent::snapshot
cargo test --offline -p harold main::tests::watch
```

- [x] **Step 4: Implement catch-up-before-ready and post-commit publication**

Project every historical page to stream head, complete the durable legacy-candidate repair gate described in the Task 12 correction record, load the clean DB snapshot, seed the hub/runtime, then expose gRPC. Each watch subscribes first and sends that receiver's current value before `changed()`. Bridge via approved `tokio_stream::wrappers::ReceiverStream`; shutdown ends forwarders.

- [x] **Step 5: Run green**

```bash
cargo test --offline -p harold agent::snapshot
cargo test --offline -p harold main::tests
cargo test --offline -p harold projector::tests
```

- [x] **Step 6: Commit `feat: stream current agent snapshots`**

Execution record: RED exited 101 on the absent hub, publisher, startup seed, and watch-service fields. GREEN added retained snapshot publication, catch-up-before-ready, snapshot-first `ReceiverStream`, reconnect/coalescing, public presence-aware summary mapping, and bounded multi-stream shutdown. Independent review found a post-commit snapshot-reload retry hole and non-agent checkpoint revision drift; `7f2717a` compares the committed checkpoint with the retained hub after every successful cycle, including no-op retries. Final snapshot 2/2, watch 3/3, projector 9/9, API contract 4/4, Harold 118/118, Clippy/audit/no-Cargo/diff gates passed and independent re-review approved `adae92a` and `7f2717a`.

### Task 10: `ReportAgentState` RPC and hook/config integration

**Files:** Modify `harold/src/main.rs`, `harold/src/main_tests.rs`, `hooks/shared/harold_turn_complete.py`, `harold/config/default.toml`, `harold/config/local.template.toml`, and `Makefile`; create `hooks/shared/harold_turn_complete_test.py` if hook tests do not already exist.

**Interfaces:** Malformed pane/state/adapter -> `INVALID_ARGUMENT`; unresolved live incarnation -> `FAILED_PRECONDITION`; append/runtime unavailable -> `UNAVAILABLE`; durable append -> `{ accepted: true }`.

- [x] **Step 1: Write failing unary RPC tables**

Cover `ReportAgentState.work_summary` absent (`Unchanged`), present empty (`Clear`), normalized value, oversized value, `UNSPECIFIED`/`UNKNOWN`, invalid adapter/pane, unresolved pane, and append failure. Separately prove normalized-empty `TurnCompleteRequest.last_user_prompt` records `Unchanged`. Reload events to prove normalized-only durability.

- [x] **Step 2: Observe red with `cargo test --offline -p harold main::tests::report_agent_state`**

- [x] **Step 3: Implement thin validation/translation**

Do not append in the RPC. Convert presence with `explicit_summary_update`, await runtime reply, and return bounded statuses without submitted content.

- [x] **Step 4: Keep the stop hook wire-compatible**

Retain `last_user_prompt`; add no duplicate request key. Adapter cleaning is defense in depth; server derivation/normalization is authoritative. Add `unittest` coverage for cleaning/payload preservation if no hook test exists.

- [x] **Step 5: Verify without deploying**

```bash
cargo test --offline -p harold main::tests::report_agent_state
python3 -m unittest discover -s hooks -p '*_test.py'
cargo run --offline -p harold -- --help
make -n deploy
```

Expected: dry-run uses `harold-api/proto/harold.proto`; no deploy occurs.

- [x] **Step 6: Commit `feat: accept explicit agent lifecycle summaries`**

Execution record: the literal planned Rust filter selected zero tests, so the effective `report_agent_state` filter was used and observed the intended RED (three RPC failures against the staged `UNIMPLEMENTED` handler; the legacy-normalization characterization was already green). Hook discovery RED ran two tests and exposed incomplete ESC-sequence cleaning. GREEN at `a51fb46` implements thin runtime delegation and fixed content-free status mapping, preserves presence-aware summary semantics and the five-key stop-hook payload, and adds discoverable hook sanitation tests. Fresh focused GREEN passed RPC 4/4 and hooks 2/2; Harold passed 123/123 with format, Clippy, dry-run deploy, no-Cargo, and diff gates clean. Independent Rust review found no issues and approved spec compliance and task quality.

### Task 11: End-to-end privacy, replay, and integration verification

**Files:** Modify `harold/src/main_tests.rs`, `harold/src/agent/runtime_tests.rs`, `harold/src/store_tests.rs`, and `tasks/agent-state-monitor/todo.md`.

**Interfaces:** Proves explicit/screen inputs through EventStream, projection, restart, and gRPC.

- [x] **Step 1: Test explicit summary end to end**

Temporary store/fake ports: report busy with `Some("Implement projector")`, project, watch, restart, watch. Event and both snapshots equal `Implement projector` and state busy.

- [x] **Step 2: Test fallback, clear, and incarnation replacement**

Apply summary-only screen fallback `Review tests`, explicit `Fix projector`, present-empty `ReportAgentState` clear, then new process incarnation. Effective summaries must be fallback, explicit, fallback, then absent. Send an empty legacy completion between explicit set and clear and assert it preserves `Fix projector`. Dashboard owns exact `No work summary reported` rendering.

- [x] **Step 3: Test raw-screen sentinel absence**

Feed a unique secret outside the extracted line. Search event JSON, DB text columns, captured structured logs, errors, and protobuf debug output for it; require zero matches while normalized candidate remains.

- [x] **Step 4: Guard local-search ownership**

Assert schema contains `optional string work_summary = 14;`, contains no `work_summary_updated_at_ms`, and has no RPC/field named Search/Query/search_term/query. Do not add dashboard tests here.

- [x] **Step 5: Run complete verification**

```bash
cargo fmt --all -- --check
cargo test --offline -p harold-api --all-targets
cargo test --offline -p events --all-targets
cargo test --offline -p harold --all-targets
cargo test --offline --workspace --all-targets
cargo clippy --offline --workspace --all-targets --all-features -- -D warnings
cargo check --offline --workspace
cargo build --offline --workspace --release
cargo audit --no-fetch
! rg -n 'rpc (Search|Query)|search_term|query' harold-api/proto/harold.proto
git diff --check
```

Expected: all pass; only the accepted `paste 1.0.15` warning may remain.

- [x] **Step 6: Perform bounded live tmux verification without deploy**

Use a disposable tmux session and temporary `HAROLD__STORE__PATH`; exercise busy/idle and explicit/fallback/missing summary. Inspect normalized events/snapshots and reason-code logs only—never print/attach captured pane text. Stop temporary processes and confirm watch streams close.

- [ ] **Step 6a: Perform real-provider dashboard acceptance**

Run real Claude Code, Codex, and OpenCode processes in isolated panes `%33`, `%34`, and `%35` against a temporary Harold instance. Submit distinct controlled tasks; require each dashboard row to transition Busy to Idle while retaining its own current summary, with no cross-talk. Stop and relaunch a provider to prove departure and new-incarnation reset. Fixtures and disposable fake-provider processes do not satisfy this gate.

External acceptance record: ready commit `d9a55ea` served `WatchAgentStates` and provider ingress on `127.0.0.1:50061`. Dashboard rev571 showed `%34` Codex and `%35` OpenCode simultaneously Busy with distinct current summaries; rev577 showed both Idle retaining those summaries with no cross-talk. Stopping `%35` removed it at rev578; a fresh process-local OpenCode relaunch rejoined at rev580 with a new incarnation and no prior-incarnation leakage. Claude sequential acceptance and three-way concurrency remain unpassed because `%33` is externally blocked by `Login expired` / `run /login`. The coordinator authorized durable docs/final review to proceed with those blockers recorded.

Durable repair acceptance record: commit `c27cd89` served the same store on isolated `127.0.0.1:50061`. Projection-only `AgentWorkSummaryCandidatesRepaired` events at versions 591 through 594 independently cleared the legacy exact configured idle-placeholder screen candidates for `%1`, `%13`, `%14`, and `%23`; the dashboard was clear by revision 594. A same-store replay and restart retained the cleared result. This evidence remains valid but does not satisfy the blocked Claude or three-provider checks.

Completion-review correction record: independent review of `c27cd89` rejected three Important findings. Corrective commit `7523735` projects all historical pages and appends/projects one all-pane repair batch before constructing the hub or runtime seed; rejects lifecycle/completion exact configured placeholders before event serialization; and therefore removes the ingress/repair pair that could straddle a 500-event projector page. All three reported findings are fixed and focused/full code gates were recorded green.

Re-review correction record: independent re-review rejected `7523735` because `unknown`, stale, and removed provider IDs bypassed repair or ingress rejection. Commit `1568fd0` uses provider-specific exact matching for a currently configured provider and conservative all-configured exact matching only when the provider ID is missing, `unknown`, stale, or removed. RED covered a removed-provider row in the first Watch snapshot, resolved-unknown lifecycle, and resolved plus tracked-unresolved unknown completion. Coverage also preserves the known-provider cross-provider guard and legitimate containing text. Those regressions and the fresh 204-test workspace gate are green; the scoped reviewer and coordinator returned explicit thumbs-up with no remaining findings.

- [x] **Step 7: Record exact outcomes in `todo.md`**

Execution record: initial verification commit `f292f2e` added durable replay/restart, summary precedence, 160-scalar, sentinel, and schema-surface coverage, with full offline and disposable-tmux gates green. Independent review rejected two split-boundary tests and the missing durable outcome record. Fix `4f2f54c` drives raw successful/failed capture through the real visible-screen adapter and serialized runtime into EventStream/projection/log/error/protobuf sinks, routes explicit set/legacy preserve/present-empty clear through the generated gRPC service methods, and records exact Task 11 outcomes. Re-review marked all three findings addressed and approved the slice. Focused controller reruns passed 2/2; the isolated committed slice passed Harold 125/125 and workspace 181/181, while the shared checkout's 127/183 count included two concurrent screen tests. No production, dependency, Cargo, or deploy change occurred.

### Task 12: Completion review and durable documentation

**Files:** After verification, modify `README.md`, `docs/explanations/architecture.md`, and `tasks/agent-state-monitor/todo.md`; create `docs/references/agent-monitor/README.md` and `docs/how-tos/setup-agent-monitor-hooks.md`.

**Interfaces:** Produces docs matching shipped code, not speculative plan text.

- [x] **Step 1: Request mandatory completion review**

Dispatch `review_subagent` over the full diff. Require correctness, privacy/leakage, protobuf compatibility, Turso transactionality, outbox selectivity, shutdown, dependencies, and test adequacy review.

Execution record: the review of `c27cd89` returned three Important findings, addressed by `7523735`. Re-review rejected that correction's missing/unknown/stale-provider handling; `1568fd0` addresses the finding with provider-specific matching and a conservative fallback only when provider identity is unavailable or no longer configured. Final scoped review found no Critical, Important, or Minor issues and returned an explicit thumbs-up; the coordinator independently approved the same range.

- [x] **Step 2: Resolve every finding and rerun affected tests**

Do not report done until reviewer gives a thumbs-up. Behavior/interface changes must update the spec and plan record.

- [x] **Step 3: Use the Documenting workflow**

Document exact events/projection/protobuf, hooks, legacy non-destructive empty behavior, explicit clear behavior, independent screen observations, timestamp-based summary precedence, durable exact-placeholder candidate repair, exact missing copy, privacy boundary, provider limitations, bounded real-provider outcomes, and dashboard-local search. Remove claims not borne out by final code.

- [x] **Step 4: Re-run final gates**

```bash
cargo fmt --all -- --check
cargo test --offline --workspace --all-targets
cargo clippy --offline --workspace --all-targets --all-features -- -D warnings
cargo build --offline --workspace --release
cargo audit --no-fetch
git diff --check
```

- [ ] **Step 5: Commit verified docs/task record with message `docs: document agent state monitoring`**

## Staged Integration Order

1. Baseline and dependency approval.
2. Shared wire contract, preserving `TurnComplete`.
3. Pure normalization/presence semantics.
4. Named inventory/incarnation identity.
5. Screen acquisition/fallback boundary.
6. Durable events/pure reconciliation.
7. Atomic projection/selective outbox.
8. Serialized runtime/completion integration.
9. Stored snapshot/streaming RPC.
10. Lifecycle RPC and adapter/config integration.
11. Privacy/replay/live verification.
12. Completion review, fixes, then durable docs.

Each stage must be green and independently reviewable. If an interface changes, update downstream interface blocks before continuing.

## Plan Self-Review Record

- Spec coverage: each input, event, reconciliation rule, projection boundary, summary precedence/missing rule, gRPC field, local-search non-goal, privacy rule, and completion gate maps to a task.
- Type consistency: lifecycle input is `optional string work_summary = 4` and maps to `WorkSummaryUpdate`; legacy completion maps to separate `CompletionSummaryUpdate` without `Clear`; screen events carry independent optional state/summary; pane output is only `optional string work_summary = 14`.
- Dependency boundary: the coordinator-provided independent audit approved exactly `tokio-stream = { version = "=0.1.18", default-features = false }` with `ReceiverStream`; it was already locked/cached and introduced no registry package/version. The local API crate reuses locked Tonic/Prost versions.
- Storage boundary: TursoDB stays service-owned with existing WAL/NORMAL/5-second timeout; migration 003 is additive/checksum-tracked.
- Privacy boundary: raw screen text exists only inside the screen adapter; sentinel tests cover event, DB, log/error, and API escape paths.
- Placeholder scan: no unresolved behavior, type, field number, dependency version, command, or approval gate remains.
