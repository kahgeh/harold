# Claude Activity Summaries Implementation Plan

**Goal:** Display readable Claude/Sonnet activity descriptions without delaying Harold's agent monitor.

**Architecture:** Existing source observations feed bounded asynchronous generation. Results enter a dedicated durable event and a revision-checked projection; original instructions remain the fallback.

**Tech stack:** Existing Rust, Tokio, Serde, Turso, events, and installed Claude CLI. No dependency changes.

**Spec:** [spec.md](spec.md). User selected Claude/Sonnet in this session; execute the described dashboard scope.

## Global constraints

Preserve source evidence and incarnation identity, output at most 160 Unicode scalars, isolate CLI tools/hooks, keep state updates independent of inference, and keep all durable mutations in the existing event/reducer/projection flow. Leave notification and inbound routing configuration separate. Use offline Cargo commands only.

## Task 1: Configured Claude generation port

**Ownership:** `harold/src/activity_summary.rs`, its tests, `harold/src/settings.rs`, configuration defaults/template. Runtime integration owner adds the root module declaration and production wiring.

**Interface:**

```rust
pub(crate) struct ActivitySummaryInput {
    pub instruction: String,
    pub assistant_reply: Option<String>,
}

#[tonic::async_trait]
pub(crate) trait ActivitySummarizer: Send + Sync {
    async fn summarize(&self, input: ActivitySummaryInput) -> Result<String, SummaryError>;
    async fn shutdown(&self) {}
}

// ClaudeActivitySummarizer::new(ActivitySummarySettings)
// ActivitySummarySettings lives in settings.rs and is Clone.
```

- [x] Write tests for construction of bounded evidence and faithful task/outcome instructions, CLI arguments/isolation, valid output, empty/invalid output, nonzero exit, oversized output, timeout, and cancellation.
- [x] Run `cargo test --offline -p harold activity_summary` and retain the intended failing assertions.
- [x] Implement the port using Tokio child processes with piped stdin/stdout, discarded stderr, kill-on-drop and explicit wait/reaping on failure. Use an RAII temporary directory and existing dependencies only.
- [x] Invoke `claude --print --safe-mode --no-session-persistence --tools "" --model sonnet --effort low --output-format json --system-prompt <instruction>`; decode the success envelope rather than mistaking an authentication error for a description.
- [x] Add typed `[activity_summary]` defaults and validation per spec, plus enabled template configuration. Keep `[ai]` untouched.
- [x] Rerun focused tests and package formatting; report exact outcomes for review.

## Task 2: Durable eligibility and background runtime

**Ownership:** `harold/src/agent/{domain,reducer,runtime}.rs`, their tests, `harold/src/store.rs`, store tests/new migration, root `main.rs`, and a focused worker module if needed.

**Consumes:** `ActivitySummarizer`, `ActivitySummaryInput`, and `ActivitySummarySettings` from Task 1.

**Produces:** A generated-summary event with full incarnation and source revision; projection fields for source revision and generated candidate; bounded runtime scheduling.

- [x] Add reducer/store regression tests proving a generated summary applies only to its basis revision and incarnation, preserves source candidates, invalidates on newer work, survives replay, and never enters the delivery outbox.
- [x] Attempt the reducer/store RED runs, then verify behavior after integration. Initial runs were blocked by concurrent compilation errors, so no behavioral RED is claimed for those tests; see `integration-report.md`.
- [x] Add a new checksum-tracked migration and extend projection row serialization without changing previous migrations. Make source-revision transitions deterministic and identical during live processing and replay.
- [x] Derive revision identities from the actual appended event envelopes, not timestamp comparisons or assumed batch offsets. Append failures leave tracked eligibility unchanged.
- [x] Add a bounded scheduler with injectable provider, latest pending input per pane, limited concurrency, and cancellation/reaping at shutdown. Carry the full incarnation and source revision through every job/result.
- [x] Add lifecycle/completion/screen tests proving blocked inference cannot block acknowledgements, unchanged observations deduplicate, newer work and pane reuse reject stale output, limits hold, and failed result append preserves fallback.
- [x] Feed complete bounded completion prompt/reply before the 160-scalar display reduction; lifecycle input may use only the context its hook supplies. Never infer completion from instruction-only input.
- [x] Wire production settings and provider in `main.rs`; leave all prior test monitor constructors disabled by default.
- [x] Run `cargo test --offline -p harold` and resolve regressions.

## Task 3: Integration evidence, documentation, and final review

**Ownership:** Coordinator owns task records and durable documentation. Workers own code fixes in their assigned modules.

- [x] Verify runtime -> event -> projection -> snapshot with a deterministic provider; include completion outcomes and pending tests.
- [x] Exercise the production Claude port with synthetic input and record its output/latency, without invoking notification dispatch.
- [x] Update the architecture/configuration documentation with source limitations, configuration, fallback, restart, and authentication behavior.
- [x] Run `cargo fmt --all -- --check`, `cargo test --offline --workspace --all-targets`, `cargo clippy --offline --workspace --all-targets -- -D warnings`, and `cargo build --offline --release -p harold -p tmx-agent-dash`.
- [x] Inspect the final diff and ensure dependencies, events submodule, secrets, and runtime artifacts are excluded.
- [x] Obtain completion reviewer approval; fix substantive findings and re-review.
- [x] Record the final implementation, tests, and deployment status in `todo.md`.

## Baseline

`cargo test --offline -p harold --all-targets`: 148 passed, 0 failed on 2026-09-09 before source edits.
