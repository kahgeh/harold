# Tmux Agent Dashboard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task. Follow red-green-refactor and record the failing and passing command for every behavior.

**Goal:** Build a Rust Ratatui dashboard that consumes Harold's snapshot stream, shows each agent's current state and work summary, filters the live inventory locally, and switches the invoking tmux client to the selected pane.

**Architecture:** Keep `App` as a pure state machine over authoritative snapshots, search input, connection state, and selection. Put gRPC, terminal ownership/input, clocks, and tmux commands behind focused adapters. The dashboard imports the generated contract from Harold's `harold-api` crate and never performs inventory, screen capture, reconciliation, persistence, or server-side search.

**Tech Stack:** Rust 2024; Ratatui 0.30.2; Crossterm 0.29.0; Tokio 1.49.0; Tonic 0.14.5; Harold's local `harold-api` crate.

**Spec:** `tasks/tmux-agent-dashboard/spec.md`

## Global Constraints

- Run the Rust supply-chain audit before the first manifest edit or dependency fetch. The 2026-08-23 audit approved the exact versions and features shown in Task 1.
- No `clap`, `async-trait`, `futures`, snapshot-testing, tracing, or local persistence dependency.
- Treat every gRPC, tmux, endpoint, clock, and error string as untrusted until sanitized and bounded.
- Key rows and selection by the full `(pane_id, pane_pid, agent_pid, agent_started_at_ms, provider_id)` incarnation.
- Search filters the current client snapshot only; it never changes the Harold API.
- Raw pane contents and evidence provenance never enter this repository.
- `AgentPaneState.work_summary = 14` is the only summary field: it has proto3 optional presence, no summary timestamp, a 160-Unicode-scalar display bound, and exact missing copy `No work summary reported`.
- Preserve `server_time_ms` and repeated monitor-health entries in the domain. A connected stream with degraded monitor health remains live and preserves rows but must not render as healthy.
- Every production behavior starts with a test observed failing for the intended reason.

---

### Task 1: Scaffold the audited crate and command-line contract

**Files:**
- Create: `Cargo.toml`
- Create: `src/lib.rs`
- Create: `src/cli.rs`
- Create: `.gitignore`

**Interfaces:**
- Produces: `cli::Options { endpoint: String }`
- Produces: `cli::parse_args<I, S>(args: I) -> Result<Options, CliError>`
- Consumes later: `harold_api` from `../harold/harold-api`

- [ ] **Step 1: Confirm the dependency audit result before editing the manifest**

Record approval for exactly:

```toml
ratatui = { version = "=0.30.2", default-features = false, features = ["crossterm_0_29"] }
crossterm = { version = "=0.29.0", default-features = false, features = ["events"] }
tokio = { version = "=1.49.0", default-features = false, features = ["macros", "rt-multi-thread", "signal", "sync", "time"] }
tonic = { version = "=0.14.5", default-features = false, features = ["channel", "codegen"] }
```

Task 1 contains only these four audited registry dependencies. Do not enable Ratatui defaults or Crossterm `event-stream`, `osc52`, `serde`, or `use-dev-tty`. Defer the `harold-api` path dependency until Task 4, after Harold creates the crate.

- [ ] **Step 2: Create the manifest, library boundary, and failing CLI tests**

Test these exact cases in `src/cli.rs` before writing the parser:

```rust
#[test]
fn defaults_to_loopback_harold() {
    assert_eq!(parse_args(["tmx-agent-dash"]).unwrap().endpoint, "http://127.0.0.1:50060");
}

#[test]
fn accepts_one_explicit_endpoint() {
    assert_eq!(
        parse_args(["tmx-agent-dash", "--endpoint", "http://127.0.0.1:6000"]).unwrap().endpoint,
        "http://127.0.0.1:6000"
    );
}

#[test]
fn rejects_missing_or_unknown_arguments() {
    assert!(parse_args(["tmx-agent-dash", "--endpoint"]).is_err());
    assert!(parse_args(["tmx-agent-dash", "--wat"]).is_err());
}

#[test]
fn rejects_invalid_endpoint_syntax() {
    assert!(parse_args(["tmx-agent-dash", "--endpoint", "://bad"]).is_err());
}
```

- [ ] **Step 3: Run the CLI test and observe the intended failure**

Run: `cargo test cli::tests -- --nocapture`

Expected: compilation fails because `parse_args` and its result types do not exist.

- [ ] **Step 4: Implement only the argument parser and endpoint validation**

`src/lib.rs` exports modules. Parse values accepted by `std::env::args_os()` and reject invalid endpoint syntax through `tonic::transport::Endpoint::from_shared`. Create the executable entry point only in Task 7, once its `run(options)` boundary exists, so this task never requires a production runtime stub.

- [ ] **Step 5: Prove the CLI slice is green**

Run: `cargo test cli::tests -- --nocapture`

Expected: all CLI tests pass.

---

### Task 2: Normalize every external display string

**Files:**
- Create: `src/text.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces: `sanitize_display(input: &str, max_scalars: usize) -> String`
- Produces: `normalize_search(input: &str) -> String`
- Produces: `display_work_summary(summary: Option<&str>) -> &str`

- [ ] **Step 1: Write failing sanitizer and fallback-copy tests**

Cover C0, DEL, C1, complete CSI/OSC/DCS escape sequences, malformed/truncated ESC sequences, printable Unicode preservation, scalar truncation, lowercase search normalization, and:

```rust
#[test]
fn missing_or_empty_summary_has_one_operator_copy() {
    assert_eq!(display_work_summary(None), "No work summary reported");
    assert_eq!(display_work_summary(Some("")), "No work summary reported");
    assert_eq!(display_work_summary(Some("Project agent snapshots")), "Project agent snapshots");
}
```

- [ ] **Step 2: Observe the focused test fail**

Run: `cargo test text::tests -- --nocapture`

Expected: failure because the text module has no implementation.

- [ ] **Step 3: Implement a dependency-free terminal-control parser**

Use a small state machine, not a regex: printable characters pass through; C0, DEL, and C1 are removed; ESC introduces CSI/OSC/DCS control sequences that are consumed through their standard terminator or end of input. Count Unicode scalar values after stripping and truncate without splitting UTF-8.

- [ ] **Step 4: Prove sanitizer behavior and run Miri-friendly unit tests**

Run: `cargo test text::tests -- --nocapture`

Expected: all text tests pass with no captured hostile suffix in failure output.

---

### Task 2A: TUI-first visible checkpoint

Execute this checkpoint before the deeper snapshot/search reducer in Task 3. It establishes a user-inspectable vertical slice without waiting for `harold-api` and without adding dependencies.

**Files:**
- Create: `src/app.rs` with only the stable domain/view types needed by the fixture
- Create: `src/ui.rs`
- Create: `examples/dashboard_demo.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces: representative `App`, snapshot, row, search, connection, and monitor-health values usable by the renderer
- Produces: `ui::render(frame: &mut Frame, app: &App, now_ms: i64)`
- Produces: a dependency-free `dashboard_demo` example using the existing audited Ratatui and Crossterm crates

- [x] **Step 1: Write the wide TestBackend RED first**

Assert a representative wide dashboard contains `BUSY`, `IDLE`, `UNKNOWN`, `WORK SUMMARY`, active `/ event` search, `2 OF 3`, selected `CURRENT WORK` detail, `LIVE`, and `MONITOR DEGRADED`; assert it contains no `EVIDENCE`, `HOOK`, or `SCREEN` labels.

Run: `cargo test ui::tests::wide_dashboard -- --nocapture`

Expected: compilation fails because the App/domain fixture types and renderer do not exist.

- [x] **Step 2: Add only the minimum stable App/domain types and renderer**

Implement the representative wide layout from [dashboard-visual.html](dashboard-visual.html). The fixture remains a complete authoritative snapshot; local filtering derives the two visible rows from the active query. Keep transport `LIVE` distinct from the degraded monitor banner and preserve the rows.

- [x] **Step 3: Prove the wide renderer GREEN**

Run: `cargo test ui::tests::wide_dashboard -- --nocapture`

Expected: the semantic buffer assertions pass.

- [x] **Step 4: Add and verify the live development example**

Create `cargo run --example dashboard_demo`. It enters the alternate screen with raw mode, draws the same representative state, exits only on `q`, and restores raw mode, cursor visibility, and the primary screen on exit. `Esc` follows the same search-clear contract as the production application. Keep cleanup local to the example; the production terminal runtime remains Task 7.

Run focused tests and build the example in dashboard implementation pane `%23`, but do not launch the interactive TUI there. Report `DEMO READY`; the coordinator launches `cargo run --example dashboard_demo` in dedicated test pane `%26` and owns the visible inspection session. Resume Task 3 only after that visual checkpoint is inspected.

---

### Task 3: Build the snapshot, selection, and local-search functional core

**Files:**
- Modify: `src/app.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces: `AgentIncarnation`, `AgentState`, `AgentRow`, `Snapshot`, `ConnectionState`, `SearchState`, `App`
- Produces: `App::begin_connection()`, `apply_first_snapshot`, `apply_later_snapshot`, `mark_disconnected`, `handle_key`, `visible_rows`
- Produces effects: `Effect::{None, Navigate { pane_id }, Retry, Quit}`

Use these stable domain shapes. Do not add a work-summary timestamp that is absent from the shared protobuf contract:

```rust
pub struct AgentIncarnation {
    pub pane_id: String,
    pub pane_pid: u32,
    pub agent_pid: u32,
    pub agent_started_at_ms: i64,
    pub provider_id: String,
}

pub struct AgentRow {
    pub incarnation: AgentIncarnation,
    pub provider_display_name: String,
    pub tmux_target: String,
    pub session_name: String,
    pub window_index: u32,
    pub pane_index: u32,
    pub working_directory: String,
    pub work_summary: Option<String>,
    pub state: AgentState,
    pub last_transition_at_ms: i64,
}

pub enum MonitorHealthState {
    Healthy,
    Degraded,
    Unknown,
}

pub struct MonitorHealth {
    pub component: String,
    pub state: MonitorHealthState,
    pub reason_code: String,
    pub observed_at_ms: i64,
}

pub struct Snapshot {
    pub through_event_version: u64,
    pub server_time_ms: i64,
    pub monitor_health: Vec<MonitorHealth>,
    pub rows: Vec<AgentRow>,
}
```

- [x] **Step 1: Write failing table-driven snapshot tests**

Cover first-snapshot authority at any revision, same-stream duplicate ignore, same-stream regression rejection, retained `server_time_ms`, healthy/degraded/unknown monitor states, connected-but-monitor-degraded snapshots preserving rows, monitor recovery on a later accepted snapshot, deterministic state/session/window/pane sorting, duplicate pane/incarnation rejection, stable-incarnation selection preservation, replacement-incarnation reset, nearest selection after removal, and empty snapshots.

- [x] **Step 2: Observe snapshot tests fail**

Run: `cargo test app::tests::snapshot -- --nocapture`

Expected: failures because `App` does not yet apply snapshots.

- [x] **Step 3: Implement the minimal snapshot state transitions**

Keep the owned full snapshot separate from derived visible indices. Do not delete filtered rows or mutate Harold state. Scope revision monotonicity to one open stream.

- [x] **Step 4: Prove snapshot tests pass**

Run: `cargo test app::tests::snapshot -- --nocapture`

Expected: all snapshot cases pass.

- [x] **Step 5: Write failing incremental-search and key-mode tests**

Cover `/`, printable `q` while editing, Unicode Backspace, Enter accepting the query, Esc clearing and leaving search editing, Esc clearing an accepted filter outside editing, Esc as a no-op when no filter exists, `q` as the sole normal quit key outside editing, case-insensitive matches over provider/summary/target/directory, streaming updates while filtered, first-match selection when the selected row is hidden, navigation restricted to visible rows, visible/total counts, and no matches.

- [x] **Step 6: Observe search tests fail**

Run: `cargo test app::tests::search -- --nocapture`

Expected: the query and visible selection do not yet change.

- [x] **Step 7: Implement search as a derived view and key state machine**

Normalize the query once per edit and each row's four searchable fields once per accepted snapshot. Keep `SearchState { query, editing }` client-local across later snapshots and reconnect attempts.

- [x] **Step 8: Prove the complete core is green**

Run: `cargo test app::tests -- --nocapture`

Expected: all snapshot, selection, search, and key-effect tests pass.

---

### Task 4: Map Harold's generated API into the trusted domain

**Files:**
- Modify: `Cargo.toml`
- Create: `src/api.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `harold_api::harold::{harold_client::HaroldClient, AgentMonitorHealth, AgentPaneState, AgentStateSnapshot, MonitorHealthState as ProtoMonitorHealthState}`
- Produces: `map_snapshot(proto) -> Result<app::Snapshot, ProtocolError>`
- Produces: `SourceStream`, owning the snapshot receiver and its cancellable reader task
- Produces: `AgentStateSource::open() -> Pin<Box<dyn Future<Output = Result<SourceStream, SourceError>> + Send + '_>>`

- [x] **Step 0: Add the generated path dependency only after it exists**

After Harold creates the shared crate, add `harold-api = { path = "../harold/harold-api" }`. Do not add or fetch any other dependency.

- [x] **Step 1: Write failing protobuf-mapping tests**

Construct generated messages directly. Cover `server_time_ms`, every pane and monitor-health enum value, unknown enum fallback to `Unknown`, every incarnation and sort field, `work_summary = None`, `Some("")`, and `Some(value)` at the 160/161-scalar boundary, invalid zero PIDs and negative process-start/transition timestamps, duplicate pane IDs, terminal controls, and all field bounds. Assert the domain has no summary timestamp. Do not specify negative or overflowing PID fixtures: protobuf exposes both PIDs as `u32`, so those fixtures cannot be constructed.

- [x] **Step 2: Observe mapping tests fail**

Run: `cargo test api::tests -- --nocapture`

Expected: mapping functions are absent.

- [x] **Step 3: Implement mapping and a channel-backed source adapter**

Connect with the validated endpoint, call `WatchAgentStates`, spawn one reader task, call `Streaming::message`, map before sending, and close the channel on stream end. Return an owned stream handle whose `Drop` cancels and joins/aborts that reader so `r`, reconnect, and runtime shutdown cannot leak a blocked gRPC task. Map `server_time_ms` and every monitor-health entry before application. Sanitize before constructing the domain; bounds are target/provider 256, directory 1024, summary 160, monitor component/reason code 64, and transport/tmux error text 512 Unicode scalar values.

- [x] **Step 4: Prove the API boundary is green**

Run: `cargo test api::tests -- --nocapture`

Expected: all mapping and fake-source tests pass.

---

### Task 5: Implement the single tmux navigation effect

**Files:**
- Create: `src/navigation.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces: `TmuxNavigator::discover_client() -> Result<Option<String>, NavigationError>`
- Produces: `PaneNavigator::jump_to(&self, client: &str, pane_id: &str) -> Result<(), NavigationError>`

- [x] **Step 1: Write failing command-construction and error-sanitization tests**

Inject a `CommandRunner` test port. Assert exact argv, never a shell string:

```text
tmux display-message -p #{client_name}
tmux switch-client -c <captured-client> -t <pane-id>
```

Cover missing tmux client, vanished pane, non-zero status, hostile stderr, and no optimistic row deletion.

- [x] **Step 2: Observe tests fail**

Run: `cargo test navigation::tests -- --nocapture`

Expected: the navigator and command runner do not exist.

- [x] **Step 3: Implement `std::process::Command` with separate argv**

Capture the invoking client once before alternate-screen entry. Reject empty client/pane values, cap sanitized stderr at 512 scalars, and leave state mutation to later snapshots.

- [x] **Step 4: Prove navigation behavior**

Run: `cargo test navigation::tests -- --nocapture`

Expected: all command and failure tests pass.

---

### Task 6: Complete the approved responsive Ratatui information hierarchy

**Files:**
- Modify: `src/app.rs`
- Modify: `src/ui.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces: `ui::render(frame: &mut Frame, app: &App, now_ms: i64)`
- Consumes only: immutable trusted `App` state

- [x] **Step 1: Write failing `TestBackend` buffer tests**

Extend the Task 2A semantic coverage with failing tests for medium, compact, undersized, loading, unavailable, stale, healthy live, monitor-unknown, monitor-recovered, empty live, no-match, missing-summary, and selection-change states. Assert semantic content, not every cell. Preserve the existing wide assertions for `WORK SUMMARY`, `CURRENT WORK`, active search, visible/total counts, retained rows, degraded component/reason text, and the absence of `EVIDENCE`, `HOOK`, and `SCREEN` labels.

- [x] **Step 2: Observe render tests fail**

Run: `cargo test ui::tests -- --nocapture`

Expected: the representative wide renderer exists, but one or more newly specified responsive/state assertions fail for missing behavior.

- [x] **Step 3: Implement responsive terminal layouts**

Extend the Task 2A renderer using [dashboard-visual.html](dashboard-visual.html). Use text labels and selection markers in addition to color. Render transport status and monitor health separately so a connected degraded or unknown monitor cannot look healthy; preserve the last committed rows under a degraded warning. Preserve state/provider/target at narrow widths; truncate summary rather than remove it. Render a resize instruction below the documented minimum.

- [x] **Step 4: Prove every render state**

Run: `cargo test ui::tests -- --nocapture`

Expected: all deterministic buffer assertions pass.

---

### Task 7: Orchestrate reconnects, input, terminal restoration, and signals

**Files:**
- Create: `src/runtime.rs`
- Create: `src/terminal.rs`
- Create: `src/main.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces: `terminal::TerminalGuard` with staged acquisition and idempotent restoration; `Drop` is the final backstop
- Produces: `runtime::run(options, source, navigator) -> Result<(), AppError>`

- [x] **Step 1: Write failing runtime tests with fake channels and clock**

Cover startup failure, first snapshot, later snapshot, connected-but-monitor-degraded snapshot with rows preserved, monitor recovery, duplicate/regression, disconnect-to-stale, 250ms/500ms/1s/2s/4s/5s capped retries, `r` cancelling delay or the current stream without leaking its reader task, snapshots while filtering, navigation failure status, quit, SIGINT, and SIGTERM. Terminal tests cover failure after each acquisition step (raw mode, alternate screen, hidden cursor), panic, render failure, explicit restoration errors, exactly-once cleanup attempts across explicit cleanup, `Drop`, and the panic hook, and continuation to later cleanup operations after one fails.

- [x] **Step 2: Observe runtime tests fail**

Run: `cargo test runtime::tests -- --nocapture`

Expected: runtime orchestration does not exist.

- [x] **Step 3: Implement the effectful shell**

Use a bounded Tokio `mpsc` channel for terminal events from a polling thread; do not enable Crossterm's async event-stream feature. `tokio::select!` over source messages, terminal events, retry timers, and Unix shutdown signals. Acquire terminal modes one step at a time and roll back completed steps if a later step fails. Share an exactly-once cleanup-attempt state with a scoped panic hook, restore the previous hook when the guard is finished, perform explicit fallible restoration on normal/error exits without short-circuiting later cleanup operations, and retain idempotent `TerminalGuard::drop` as the final backstop.

- [x] **Step 4: Prove runtime behavior and terminal cleanup**

Run separately:

```bash
cargo test runtime::tests -- --nocapture
cargo test terminal::tests -- --nocapture
```

Expected: all orchestration tests pass; each fake terminal cleanup operation is invoked at most once, later operations still run after an injected failure, and every non-failing operation restores its mode.

---

### Task 8: Document, integrate, and perform live proof

**Files:**
- Create: `README.md`
- Modify: `tasks/tmux-agent-dashboard/todo.md`
- Modify: `tasks/tmux-agent-dashboard/spec.md` only where implementation evidence changes a claim

- [ ] **Step 1: Document installation, endpoint, keys, search, state semantics, stale behavior, and navigation limits**

Include the exact missing-summary copy, the fact that search is local, the loopback-default security boundary, and that raw screen/evidence data never reaches the dashboard.

- [ ] **Step 2: Run the complete automated verification suite**

Run, in order:

```bash
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
git diff --check
```

Expected: every command exits zero with no warnings.

- [ ] **Step 3: Prove HTML reference consistency**

Parse with headless Chrome and render at 1440×1000 and 760×1100. Confirm search, visible/total count, work-summary table, current-work detail, keyboard hints, responsive stacking, and no visible evidence provenance.

- [ ] **Step 4: Run a live integration session**

Against a temporary Harold state directory and disposable tmux session, prove: immediate initial snapshot, 160-scalar summary handling and exact missing copy, connected-but-monitor-degraded warning with rows preserved, monitor recovery, incremental search while a later snapshot arrives, stale retention after disconnect, authoritative reconnect after store reset, atomic cross-session `switch-client`, and vanished-pane error handling. In PTY/subprocess cases, inject failure after each terminal-acquisition step and prove cleanup for panic, render failure, SIGINT, SIGTERM, and explicit restoration failure; assert each required cleanup operation is attempted exactly once, later cleanup steps still run after one restoration operation fails, and every operation not injected to fail restores its mode.

After Tasks 6–8 wire the real input/runtime path, use `tmux send-keys` and `capture-pane` in designated test pane `%26` to prove live keyboard behavior: `j`/`k` visibly change the selected row; `/`, a query, `Enter`, and `Esc` visibly change filter/edit state; and `Enter` in a disposable isolated tmux client switches only that invoking client to the selected pane while unrelated clients remain on their original targets. Send every `Enter` key in its own separate `tmux send-keys` command, never inline with literal content. Record the exact commands, client identities, before/after pane targets, and captured results. Fake runner/unit tests do not complete the navigation acceptance criterion without this live layer.

Append every actual browser or terminal run to [screen-testing.md](screen-testing.md), the append-only evidence source of truth, with exact commands, pane/client identities, dimensions, observable captured results, pass/fail, and limitations. Keep planned work in that ledger's required-tests section until it is actually run; a todo checkbox alone is not verification evidence.

- [ ] **Step 5: Update the task review with exact evidence**

Record commands, pass counts, live targets used, deviations from the plan, dependency audit result, and remaining limitations. Do not mark unchecked acceptance criteria complete from inference.

- [ ] **Step 6: Request independent completion review**

The reviewer must inspect both repositories, the shared protobuf contract, tests, command output, HTML renders, privacy boundary, and live integration evidence. Resolve every substantive finding and obtain an explicit thumbs-up before reporting completion.
