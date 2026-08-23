# Agent State Monitor Backend

## Specification

- [x] Inspect Harold's durable event stream, application-state database, projector, tmux discovery, hooks, and gRPC server.
- [x] Define durable pane, lifecycle, screen-evidence, departure, and monitor-health events.
- [x] Define incarnation-safe projection and hook/screen precedence.
- [x] Define stored current state and snapshot-then-stream consumer behavior.
- [x] Define gRPC, configuration, privacy, sanitization, failure, and verification contracts.
- [x] Record the approved correction that the dashboard shows a concise current-work summary, not evidence provenance.
- [x] Define explicit-summary precedence, provider-specific visible-screen fallback, 160-scalar normalization, durable projection, and exact `No work summary reported` absence copy.
- [x] Keep public `work_summary` presence-aware and omit `work_summary_updated_at_ms` because the dashboard has no consumer requirement.
- [x] Route provider-specific screen-summary fallback through the same sanitizer, bound, durable event stream, and projection timestamp semantics.
- [x] Keep search dashboard-local; Harold exposes snapshot summary fields but no search contract.
- [x] Preserve `TurnCompleteRequest.last_user_prompt` as the stop-hook summary source and add `ReportAgentStateRequest.optional work_summary = 4` for active lifecycle hooks.
- [x] Treat normalized-empty legacy `TurnCompleteRequest.last_user_prompt` as `Unchanged`; only present-empty `ReportAgentState.work_summary` clears.
- [x] Model `AgentScreenObserved` with independent optional state and optional fallback summary, appending when either conclusive value changes.
- [x] Obtain user approval to proceed from specification correction into implementation planning.

## Implementation planning

- [x] Create `implementation-plan.md` with test-first tasks, exact files, interfaces, protobuf field numbers, staged integration, and verification commands.
- [x] Record coordinator-provided independent Rust audit approval for `tokio-stream = { version = "=0.1.18", default-features = false }` using `ReceiverStream`; the approved crate/version is already locked and cached and adds no package/version.

## Implementation

- [x] Implement and commit the shared `harold-api` contract at `7e226b7`; legacy `TurnComplete` tags remain 1-5, optional summary presence round-trips, staged new RPCs return `UNIMPLEMENTED`, and descriptor tests prove there is no search, provenance, evidence, or public summary-timestamp surface.
- [x] Implement named provider inventory and visible-screen classifiers.
- [x] Implement explicit current-work summaries and provider-specific visible-screen fallback without persisting or logging raw screen content.
- [x] Implement durable observation events and the application-state projection.
- [x] Implement monitor health, snapshot publication, and gRPC streaming.
- [x] Implement lifecycle hook adapters and legacy configuration compatibility.
- [x] Keep dashboard search out of Harold and publish only the effective optional summary.
- [x] Update durable Harold documentation.

## Verification

- [x] Prove every RED/GREEN cycle in authorized Tasks 6-9 and retain the exact execution records in `implementation-plan.md` and the SDD reports.
- [x] Run Tasks 6-9 formatting, focused tests, workspace tests, Clippy, check, release build, no-fetch audit, forbidden search-surface scan, and diff/Cargo-boundary gates.
- [x] Obtain independent Rust-review thumbs-up for each Tasks 6-9 coherent slice after resolving every finding.
- [x] Prove every red-green-refactor cycle required by the implementation plan; Task 11 is verification-only and required no artificial behavioral RED because prerequisite production behavior was already present.
- [x] Run formatting, focused tests, workspace tests, Clippy, docs, audit, and release/deploy checks.
- [x] Verify live inventory, busy/idle transitions, reconnect, restart, and graceful shutdown with a disposable tmux session, temporary store, isolated gRPC port, zero raw-pane output, zero sentinel matches, and confirmed session/process/port/watch cleanup.
- [x] Obtain the required independent completion-review thumbs-up.

### Task 11 verification outcomes

- [x] Prove complete incarnation identity and explicit `Implement projector` through durable events, projection, snapshot-first `WatchAgentStates`, store reopen, and restart watch.
- [x] Prove provider-screen fallback, gRPC explicit set, legacy normalized-empty preserve, gRPC present-empty clear back to fallback, and same-pane new-incarnation reset without inherited summary.
- [x] Feed a unique secret outside the extracted summary line through the real `TmuxVisibleScreen` adapter and serialized runtime; retain only normalized `Review tests` and find zero sentinel matches in event JSON, stored snapshot/database files, structured logs, bounded errors, protobuf debug, and protobuf wire bytes.
- [x] Prove 160-Unicode-scalar normalization and that the public schema exposes only optional `work_summary = 14`, with no search/query, provenance, or summary timestamp surface.
- [x] Complete the bounded live check without deploy: fallback Busy, explicit Busy, clear-to-fallback Idle, replacement Idle with missing summary, durable normalized values, zero sentinel matches, graceful watcher close, and no remaining temporary session/process/port/path.
- [x] Real-provider two-way acceptance at ready `d9a55ea`: dashboard rev571 showed `%34` Codex and `%35` OpenCode simultaneously Busy with their own distinct current summaries; rev577 showed both Idle retaining those summaries with no cross-talk.
- [x] Real-provider departure/rejoin acceptance: stopping `%35` removed its row at rev578; process-local OpenCode relaunch rejoined at rev580 with a new incarnation and no prior-incarnation leakage.
- [x] Same-store durable repair acceptance at `c27cd89`: projection-only repair events 591-594 cleared legacy exact configured idle-placeholder screen candidates for `%1`, `%13`, `%14`, and `%23`; the dashboard was clear by rev594 and the candidates remained clear after replay/restart against the same store.
- [ ] Real Claude Code sequential acceptance in `%33` — externally blocked by `Login expired` / `run /login`; do not claim passed.
- [ ] Real three-provider concurrency across `%33`/`%34`/`%35` — externally blocked by the same Claude authentication state; do not claim passed.
- [x] Pass the fresh Task 11 gate: harold-api 4/4, events 52/52, Harold 127/127, workspace 183/183, format, Clippy with warnings denied, check, release build, no-fetch audit, forbidden search-surface scan, Cargo-boundary check, and diff check.
- [x] Complete Task 12 independent final review and migrate verified durable documentation.

### Task 12 documentation outcomes

- [x] Reconcile the durable docs against the canonical protobuf, corrective `1568fd0` runtime/reducer/projector/store code, migration, named defaults, hook implementations, provider-integration report, legacy-repair report, and bounded live acceptance record.
- [x] Add the agent-monitor reference with an aligned ASCII architecture diagram, full Mermaid interactions, durable events, projection, reconciliation, RPC, configuration, privacy, lifecycle, and failure contracts.
- [x] Add the hook/configuration how-to with prerequisites, numbered setup, verification, and troubleshooting without claiming that deployment occurred.
- [x] Update the top-level documentation entry points and the architecture explanation without duplicating the reference and how-to contracts.
- [x] Verify local documentation links/paths, Markdown whitespace, and ASCII visual widths.
- [x] Pass the fresh Task 12 formatting, focused hook/API tests, workspace tests, Clippy, release build, no-fetch audit, dry-run deploy, Cargo-boundary, and diff gates.
- [ ] Commit only the owned durable docs and task record as `docs: document agent state monitoring`.
- [x] Obtain current-code completion re-review after corrective `1568fd0`; the scoped reviewer and coordinator both returned explicit thumbs-up with no remaining findings.
- [ ] Restart isolated `50061` on the documented ready revision and repeat the same-store placeholder/replay proof.

## Review

The coordinator initially authorized Tasks 1-2, then independently gated and authorized Tasks 3-5 and Tasks 6-9. The coordinator subsequently corrected the scope: Task 9 was an accepted slice rather than the completion boundary, and Tasks 10-12 are authorized through final review and durable documentation.

Task 1 baseline: Harold 37/37 tests passed; events 52/52 tests passed; `cargo audit --no-fetch` exited 0 with only the accepted transitive `paste 1.0.15` unmaintained warning (`RUSTSEC-2024-0436`).

Tasks 1-2 checkpoint: strict red first observed missing `harold-api` package (exit 101), and the staged RPC test separately observed generated-trait E0046 before the minimal stubs. Green verification passed API 4/4, Harold 38/38, events 52/52, API/Harold Clippy with warnings denied, Harold all-target check, format/diff checks, no-fetch audit, forbidden public-surface scan, unchanged registry package count (424), and deploy dry-run canonical-proto path. Independent completion review returned no findings and a thumbs-up. At that checkpoint, Task 3 was still unauthorized and untouched.

Tasks 3-5 checkpoint: Task 3 committed as `fd0b93e`, Task 4 as `4b27040`, and reviewed Task 5 as `22a24aa`. Strict REDs covered missing normalization/screen modules, invalid presence conversions, missing named inventory, privacy-bearing `Debug`, incomplete C1/control-string sanitation, reserved identity collision, typed ambiguous resolution, and missing provider clauses. Post-fix GREEN passed summary 5/5, settings 6/6, inventory 11/11, tmux 3/3, inbound 12/12, screen 6/6, API contract 4/4, Harold 63/63, Clippy with warnings denied, format, forbidden public-surface checks, no-Cargo-diff gate, and diff checks. Independent Rust review returned no remaining findings and approved the slice. No dependency or Cargo file changed. OpenCode 1.4.10 has verified state clauses but no summary prefix because its live prompt and user-message rows share the same visible prefix; enabling it would persist placeholder copy as a false work summary. Task 6 remains untouched pending coordinator review.

Tasks 6-9 checkpoint: Task 6 committed as `072e7fb`, `4e22eae`, and `afb0126`; Task 7 as `f154843` and `bd883bf`; Task 8 as `1725f2e`, `1715755`, `2cc6682`, and `b81438c`; Task 9 as `adae92a` and `7f2717a`. Strict RED/GREEN covered durable event/reducer contracts, lifecycle-epoch screen reconciliation, atomic rollback/replay, configured grace, non-blocking consistent snapshots, runtime restart seeding, permanent acquisition wedges, health degradation/recovery, metadata sanitation and restart equivalence, tick coalescing, zero-grace equal-millisecond ordering, snapshot-first watch behavior, post-commit reload retry, and non-agent checkpoint publication. Every independent Rust-review finding was fixed and re-reviewed to a thumbs-up.

Task 10 checkpoint: `a51fb46` implements the thin `ReportAgentState` ingress and wire-compatible stop-hook sanitation. Strict RED/GREEN covered optional summary preserve/clear/set, the 160-scalar bound, invalid state/pane/adapter, unresolved incarnation, append/runtime failure, durable normalized reload, non-destructive legacy empty completion, complete terminal-sequence cleaning, and exact five-key hook payload preservation. Fresh RPC 4/4 and hook 2/2 checks passed; Harold passed 123/123 with format, Clippy, help, dry-run deploy, no-Cargo, and diff gates clean. Independent Rust review returned no findings and approved both spec compliance and task quality.

Task 11 checkpoint: initial commit `f292f2e` added replay, streaming, normalization, privacy, and schema integration coverage. Review fix round 1 replaced typed screen-event injection with a real `TmuxVisibleScreen` command adapter feeding the serialized runtime, including bounded failed-capture diagnostics, and routed explicit set, legacy-empty preserve, and present-empty clear through the actual gRPC ingress before replacement reset. The first real-adapter run exposed only a test-fixture prefix mismatch and was rejected as behavioral RED; after aligning the fixture with the configured `Task: ` prefix, both corrected focused tests passed. A fresh disposable-tmux run again proved fallback Busy, explicit Busy, clear-to-fallback Idle, replacement Idle with missing summary, zero sentinel matches, watcher closure, and complete active-resource cleanup. Fresh verification passed harold-api 4/4, events 52/52, Harold 127/127, workspace 183/183, format, Clippy, check, release build, no-fetch audit, forbidden search-surface scan, Cargo-boundary check, and diff check. Task 12 review and durable documentation remain open.

Provider-integration checkpoint: live dashboard rev 557 exposed stale source precedence in Codex `%34`. RED proved that a later substantive Busy screen prompt could not replace the prior completed explicit candidate. Commits `0f0817e` and `d9a55ea` select the latest durable candidate timestamp with explicit winning ties, preserve the current summary across Idle placeholder/absence, reject only exact normalized Codex placeholders at acquisition and runtime boundaries, and remove complete ESC/C1 terminal sequences in the OpenCode adapter. Combined review fix round 1 resolved both Important findings and returned a thumbs-up. Fresh combined gates passed harold-api 4/4, events 52/52, Harold 133/133, workspace 189/189, OpenCode 12/12, Python hooks 2/2, format, Clippy, check, release, no-fetch audit, schema/Cargo/diff gates, and an OpenCode 1.18.15 process-local loader smoke using `HAROLD_ADDR=127.0.0.1:50061`. Real Codex/OpenCode concurrency, Idle retention, no cross-talk, and OpenCode departure/rejoin then passed at dashboard revisions 571/577/578/580. Claude sequential and three-way concurrency remain explicitly unpassed because `%33` displayed `Login expired` / `run /login`.

Legacy-placeholder repair checkpoint: live rev580 exposed exact configured placeholders retained by historical screen candidates. Commit `c27cd89` adds projection-only `AgentWorkSummaryCandidatesRepaired` facts with full incarnation and independent clear flags. The externally observed same-store server on `127.0.0.1:50061` appended repair events 591-594 for `%1`, `%13`, `%14`, and `%23`; the dashboard was clear by rev594 and remained clear after same-store replay/restart. Independent review rejected three Important findings, fixed by `7523735`: startup projects all history and one atomic all-pane repair batch before creating the hub/runtime seed; lifecycle and completion exact placeholders become non-setting before serialization; and no ingress/repair pair can straddle a 500-event page. Re-review then rejected unknown, stale, and removed provider IDs bypassing repair/rejection. Corrective `1568fd0` uses known-provider-specific exact matching and falls back to all configured fragments only for missing, `unknown`, stale, or removed provider IDs. RED covered removed-provider first Watch, resolved-unknown lifecycle, and resolved/tracked-unresolved unknown completion; coverage also retains known-provider cross-provider isolation and containing text. Fresh workspace verification passed 204/204, and the scoped reviewer plus coordinator returned explicit thumbs-up with no remaining findings. No schema, protobuf, dependency, migration, hook, or deployed-service change occurred; the authorized current-code same-store restart remains the final live gate.

Fresh Tasks 6-9 GREEN passed Harold 118/118, harold-api contract 4/4, events unit/integration suites, workspace all-targets, workspace Clippy with all features and warnings denied, workspace check, release build, no-fetch audit, forbidden search-surface scan, no-Cargo-diff gate, and diff check. `cargo audit --no-fetch` reports only the accepted transitive `paste 1.0.15` unmaintained warning (`RUSTSEC-2024-0436`). No new dependency or Cargo edit was required; Task 9 uses the already-approved exact `tokio-stream = { version = "=0.1.18", default-features = false }` declaration.
