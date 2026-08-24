# Tmux Agent Dashboard

## Design checkpoint

- [x] Inspect the dashboard repository and current Git state.
- [x] Inspect Harold's tmux process discovery and agent configuration.
- [x] Define hook events as the primary source for busy, idle, and unknown states.
- [x] Define provider-specific screen markers and conflict precedence with hook state.
- [x] Compare standalone and Harold-owned implementation boundaries.
- [x] Define Harold's lifecycle-event input, persisted current-state projection, and snapshot-then-stream subscription contract.
- [x] Defer last-event cursor replay; the dashboard MVP reconnects from Harold's latest stored projection.
- [x] Present the proposed architecture, interaction model, and verification strategy.
- [x] Obtain design approval before scaffolding or implementation.

## Specifications

- [x] Write the Harold backend specification.
- [x] Write the dashboard client specification.
- [x] Create and render the HTML dashboard visual reference.
- [x] Obtain user review of both specifications before implementation planning.
- [x] Revise the specifications and visual to prioritize durable work summaries over evidence provenance.
- [x] Add dashboard-local incremental search to the approved behavior contract.
- [x] Align the dashboard with the final shared API: optional `work_summary = 14` only, 160-scalar bound, exact missing copy, server time, and repeated monitor health.
- [x] Specify connected-but-monitor-degraded rendering that preserves rows without presenting a healthy monitor state.

## Implementation planning

- [x] Write the approved Harold backend plan with test-first checkpoints.
- [x] Write the approved dashboard client plan with test-first checkpoints.

## Implementation

- [x] Confirm the coordinator-provided audit approval before adding the four exact registry dependencies; defer `harold-api` until Task 4.
- [x] Complete the TUI-first wide TestBackend vertical slice and hand off the verified demo command to dedicated test pane `%26`.
- [x] Implement the Harold streaming client, server-time/monitor-health mapping, and snapshot validation using red-green-refactor.
- [x] Implement dashboard connection/selection state using red-green-refactor.
- [x] Implement the Ratatui interface, reconnect loop, and tmux navigation.
- [x] Add configuration, usage documentation, and graceful error handling.
- [x] Render green `● IDLE`, orange `● BUSY`, and grey `○ UNKNOWN` status labels in Ratatui table/detail views and the HTML visual reference, retaining text for monochrome/accessibility.
- [x] Make `q` the sole normal quit key: while editing, `Esc` always clears and exits editing; outside editing it clears an accepted filter or is a no-op when no filter exists; update automated tests, TUI/HTML key hints, operator docs, and live `%26` evidence.

## Verification

- [x] Run formatting, tests, Clippy with warnings denied, and a release build.
- [x] Exercise the dashboard against live tmux panes and verify navigation; ST-017, ST-020, and ST-021 cover selection, outside-tmux authority, and isolated-client switching.
- [ ] In disposable tmux sessions/panes, start authenticated Claude Code, Codex, and OpenCode processes; send controlled non-mutating tasks; prove each appears through Harold, transitions busy to idle, and reports an accurate bounded work summary.
- [x] Use an isolated OpenCode 1.18.15 lifecycle adapter through `ReportAgentState`; ST-025, ST-026, and the reviewed ST-028 rerun use the process-local repository plugin without mutating the user's global OpenCode configuration.
- [ ] For every provider, derive `work_summary` from the most recent substantive submitted user instruction after scanning past empty/system/tool/UI entries; never use the current input composer, synthesize multiple turns, or expose idle placeholder copy.
- [ ] Run the three controlled tasks both sequentially and concurrently; prove three simultaneous busy rows return to three idle rows without summary cross-talk.
- [x] Rerun Codex and OpenCode concurrently against reviewed Harold commit `d9a55ea`; ST-028 proves both current distinct summaries while simultaneously busy and after both return idle, fixing the ST-026 Codex recency failure without cross-talk.
- [x] Stop and relaunch the disposable OpenCode process; ST-029 proves authoritative row removal/rejoin and that a new process incarnation does not inherit the departed incarnation's work summary.
- [x] Run the final reviewed durable repair from `7523735` plus `1568fd04e2d04c9a73f11330b495fe173538d0fa` against the exact fixture configuration and existing store; ST-031 supersedes ST-030 and proves four persisted Codex prompt placeholders remain repaired after graceful same-store replay.
- [ ] Capture provider/pane/process identities and dashboard observations for the three-agent test, including any unsupported marker or hook behavior as an explicit failure rather than substituting fixture data.
- [x] In pane `%26`, capture live `j`/`k` selected-row changes and `/` query `Enter`/`Esc` filter/edit changes using `tmux send-keys` plus `capture-pane`; send every `Enter` separately.
- [x] In an isolated disposable tmux client, prove live `Enter` navigation switches only the invoking client and leaves unrelated clients unmoved; record exact commands, identities, and before/after targets.
- [x] Append every actual browser/terminal run through the current ST-031 checkpoint to `screen-testing.md`; incomplete, rejected, superseded, and failed assertions remain explicit in the ledger.
- [x] Prove PTY cleanup for partial initialization, panic, render failure, SIGINT, SIGTERM, and explicit restoration failure, including exactly-once cleanup; ST-019 and ST-027 record the live evidence.
- [x] Request the design completion review subagent.
- [x] Address all design review findings and obtain a thumbs-up.

## Review

The initial design-review findings were corrected and the original specifications received a thumbs-up. The coordinated final contract keeps only optional `AgentPaneState.work_summary = 14`, bounds it to 160 Unicode scalar values, uses exact missing copy `No work summary reported`, and requires server-time plus monitor-health mapping. Connected-but-monitor-degraded snapshots remain live and retain rows while rendering an explicit warning. Visual, keyboard, navigation, status-colour, q-only quit, PTY lifecycle, two-provider summary-recency, and final reviewed durable placeholder-repair checks are complete. Remaining work is authenticated Claude sequential and three-provider concurrent evidence, plus final combined completion review.

## Implementation evidence

### Task 1: CLI

- [x] RED — `cargo test cli::tests -- --nocapture` exited 101 because `cli::parse_args` did not exist, which is the intended missing behavior.
- [x] GREEN — `cargo test cli::tests -- --nocapture` passed 4 tests after requiring an absolute plaintext HTTP endpoint; an intermediate run proved Tonic alone accepts relative URI syntax.

### Task 2: Display text

- [x] RED — `cargo test text::tests -- --nocapture` exited 101 because the sanitizer, search normalizer, and missing-summary function did not exist.
- [x] GREEN — `cargo test text::tests -- --nocapture` passed 7 adversarial control-sequence, Unicode, 160-scalar-bound, search-normalization, and fallback-copy tests.

### Task 2A: TUI-first visible checkpoint

- [x] RED — `cargo test ui::tests::wide_dashboard -- --nocapture` exited 101 because the App/domain fixture types and `ui::render` did not exist.
- [x] GREEN (renderer) — `cargo test ui::tests::wide_dashboard -- --nocapture` passed the representative wide semantic buffer test.
- [x] RED (review fix) — `cargo test ui::tests::wide_dashboard -- --nocapture` failed because unknown monitor health rendered as healthy instead of `MONITOR UNKNOWN`.
- [x] GREEN (review fix) — `cargo test ui::tests::wide_dashboard -- --nocapture` passed 3 tests after unknown/empty health stopped rendering as healthy; the wide fixture now also proves the retained Claude row and `inventory:tmux_unavailable` diagnostic.
- [x] RED (demo input) — `cargo test --example dashboard_demo -- --nocapture` exited 101 because the `q`/`Esc` quit predicate did not exist.
- [x] GREEN (demo input) — `cargo test --example dashboard_demo -- --nocapture` passed the `q`/`Esc` quit behavior test.
- [x] DEMO HANDOFF — fresh tests/build and completion review passed; report `DEMO READY`, then the coordinator runs `cargo run --example dashboard_demo` in pane `%26` (zoom or resize for the wide layout if needed).

### TUI-first checkpoint verification

- [x] `cargo fmt --all -- --check`
- [x] `cargo test ui::tests::wide_dashboard -- --nocapture` — 3 passed.
- [x] `cargo test --example dashboard_demo -- --nocapture` — 1 passed.
- [x] `cargo test --all-targets` — 14 library tests and 1 example test passed.
- [x] `cargo clippy --all-targets --all-features -- -D warnings`
- [x] `cargo build --example dashboard_demo`
- [x] `cargo tree --depth 1` — exactly the four approved direct dependencies; no `harold-api`.
- [x] Completion reviewer gave an explicit thumbs-up with no substantive findings.

### Task 3: Snapshot, selection, monitor health, and search core

- [x] RED (snapshot) — `cargo test app::tests::snapshot -- --nocapture` exited 101 with 18 missing-method errors for the connection and snapshot transition API.
- [x] GREEN (snapshot) — the focused snapshot suite passed 7 tests.
- [x] RED (search/key) — `cargo test app::tests::search -- --nocapture` exited 101 because `handle_key`, `Effect`, and `visible_counts` were absent.
- [x] GREEN (search/key) — the focused search suite passed 6 tests.
- [x] RED/GREEN (review regression) — the reconnect/filter assertion first failed because the hidden prior row remained selected, then passed after authoritative snapshots reselected the first visible row.
- [x] GREEN (complete core) — `cargo test app::tests -- --nocapture` passed 14 tests.
- [x] Task gate — fresh independent `cargo test --all-targets` passed 28 library tests plus 1 example test; `cargo clippy --all-targets --all-features -- -D warnings` passed.
- [x] RED (slice review) — authoritative snapshots with an active search failed 3 new cases by leaving selection `None` when visible matches existed: no prior selection, departed selection, and same-pane replacement.
- [x] GREEN (slice review) — all 4 authoritative-search tests passed after accepted snapshots reconcile a non-empty filter to the first visible row without preserving the old incarnation; the App suite now passes 17 tests.

### Task 4: Harold API mapping and source adapter

- [x] Dependency gate — the Rust supply-chain auditor approved only `harold-api = { path = "../harold/harold-api" }` at Harold commit `4b2704031da81bc0adfb86fad507674dbce6678c`; no other direct dependency was added. The audit recorded the upstream crate's broader default Tonic feature footprint as non-blocking.
- [x] RED (mapping/source) — `cargo test api::tests -- --nocapture` failed to compile because `ProtocolError`, `SourceError`, `SourceStream`, and `map_snapshot` were absent.
- [x] GREEN (initial boundary) — the focused API suite passed 7 tests.
- [x] RED (reader-path review) — the added production-path adapter tests failed to compile because `SnapshotReader` and `spawn_reader` were absent.
- [x] GREEN (complete boundary) — `cargo test api::tests -- --nocapture` passed 10 tests covering mapping, validation, bounds, EOF, delivered protocol/status errors, and cancellation of a reader blocked in `message()`.
- [x] Task gate — fresh independent `cargo test --all-targets` passed 38 library tests plus 1 example test; `cargo clippy --all-targets --all-features -- -D warnings` passed.

### Task 5: Tmux navigation

- [x] RED — `cargo test navigation::tests -- --nocapture` exited 101 because the command output/runner, errors, and navigator types were absent.
- [x] GREEN — `cargo test navigation::tests -- --nocapture` passed 6 tests covering exact argv, missing client, blank input rejection, vanished panes, bounded terminal-safe stderr, and snapshot-owned row retention.
- [x] Task gate — fresh independent `cargo test --all-targets` passed 44 library tests plus 1 example test; `cargo clippy --all-targets --all-features -- -D warnings` passed.
- [x] RED (authority review) — `cargo test navigation::tests -- --nocapture` exited 101 because the invoking-process `ProcessContext` boundary was absent; outside-tmux discovery could call tmux and resolve an unrelated attached client.
- [x] GREEN (authority review) — the focused navigation suite passed 8 tests: missing/empty/terminal-empty `TMUX` or `TMUX_PANE` returns no client with zero tmux calls, valid invoking context preserves exact argv, and command output/context ports are private with no raw-output `Debug` exposure.
- [x] LIVE GATE — after Tasks 6–8, prove outside-tmux discovery is disabled and isolated-client Enter navigation moves only the invoking client; record evidence in `screen-testing.md` before claiming navigation complete.

### Tasks 3–5 slice review

- [x] Fresh final gate after the invoking-client authority fix — `cargo fmt --all -- --check`, `cargo test --all-targets` (49 library tests plus 1 example), `cargo clippy --all-targets --all-features -- -D warnings`, `cargo tree --depth 1`, and `git diff --check` all passed.
- [x] The earlier 47-test slice approval was superseded when coordinator live proof exposed the outside-tmux authority defect; the corrected 49-test code re-review found no substantive defect.
- [x] Final authority-fix reviewer rechecked the evidence and gave explicit approval with no findings.
- [x] Stop before runtime and responsive completion Tasks 6–8; leave the inspected demo running in pane `%26` untouched.

### Task 6: Responsive renderer states

- [x] RED — the initial 16-state UI suite produced 9 intended failures for missing responsive/transport/empty-state behavior; two later compact count/query assertions each failed once before their focused fixes.
- [x] GREEN — `cargo test ui::tests -- --nocapture` initially passed 17 tests covering wide, medium, compact, undersized, loading, unavailable, stale, healthy, monitor unknown/recovered/degraded, empty, no-match, missing-summary, and selection-change states.
- [x] RED (review) — three new fixtures failed as intended: selected row below the table viewport was invisible, 140 by 18 still rendered clipped detail, and 60 by 18 clipped the revision.
- [x] GREEN (review) — `cargo test ui::tests -- --nocapture` passed 20 tests after stateful table scrolling, height-gated detail, and compact masthead fixes.
- [x] Task gate — `cargo test --all-targets` passed 66 library tests plus 1 example; formatting, Clippy with warnings denied, and diff checks passed.
- [x] RED (second review) — at 22 focused tests, the compact product-label assertion and 140 by 27 realistic wrapped-detail boundary failed while the prior 20 and tall-detail preservation passed.
- [x] GREEN (second review) — `cargo test ui::tests -- --nocapture` passed 22 tests after the 60-column abbreviated product masthead and content-aware detail fit calculation.
- [x] RED (Unicode review) — the first fixture run failed to compile because of a missing test-only `Line` import and was rejected as invalid RED; the valid odd-width CJK boundary then demonstrated Ratatui using 2 rendered rows where spill-safe layout requires 3.
- [x] GREEN (Unicode review) — `cargo test ui::tests -- --nocapture` passed 26 tests after replacing scalar/`char` packing with Ratatui `styled_graphemes` and conservative grapheme-cell measurement covering CJK, ZWJ-family emoji, and keycap/variation-selector clusters.
- [x] RED (hot-path review) — with 28 focused tests, the new zero-width measurement regression panicked in the synthetic buffer scan with `chunk size must be non-zero`; the 1,024-family ZWJ boundary already passed at 49 rows.
- [x] GREEN (hot-path review) — `cargo test ui::tests -- --nocapture` passed 29 tests after removing the area-sized buffer, scalar-derived height, and cloned lines; the direct Ratatui-grapheme counter also matches the trimmed multiple-whitespace boundary.
- [x] Final Task 6 gate — `cargo test --all-targets` passed 75 library tests plus 1 example; formatting, Clippy with warnings denied, and diff checks passed.
- [x] Final Task 6 independent re-review — no findings; explicit thumbs-up after re-running 29 focused UI tests, 75 library tests plus 1 example, formatting, warnings-denied Clippy, and diff checks.
- [x] Task 6 source-only commit — `1bbb387 feat: complete responsive dashboard renderer`; `tasks/` remained untracked and unstaged.
- [x] RED (detail allocation review) — `detail_lines_borrow_agent_text_instead_of_cloning_it` failed because the TARGET span was owned; a later out-of-bounds assertion was a test-fixture error and was not counted as RED.
- [x] GREEN (detail allocation review) — the wide path now constructs one lifetime-generic detail-line vector, borrows it for fit measurement, and moves the same vector into rendering; target, pane ID, directory, and work summary spans borrow agent text. The shared focused UI suite passed 31 tests (the prior 29, this regression, and one preserved concurrent runtime-status fixture).
- [x] Shared full warnings-denied gate after detail allocation review — superseded by the completed Task 7 gate: 99 library tests plus 1 example, package formatting, warnings-denied Clippy, build, and diff checks pass. Workspace-wide `cargo fmt --all -- --check` also inspects the sibling Harold workspace and currently reports only unrelated formatting in `../harold/harold/src/agent/runtime_tests.rs`.

### Task 7: Production runtime, terminal lifecycle, and normal binary

- [x] RED (prerequisites) — runtime compilation lacked awaited `SourceStream::close` and local snapshot-receive time; the first terminal and runtime suites failed on unresolved lifecycle/core APIs.
- [x] RED (runtime core) — the initial key-state GREEN attempt passed 5 of 6 tests and exposed missing initial selection; tmux discovery degradation initially lacked `resolve_client`.
- [x] RED (orchestration) — 9 of 11 runtime tests passed before the pending open future retained its cancellation guard; 11 of 12 passed before render errors were routed through awaited stream close.
- [x] RED (cleanup reporting) — cleanup-error merging and partial-initialization rollback-report tests first failed on missing reporting paths.
- [x] GREEN (runtime) — `cargo test runtime::tests -- --nocapture` initially passed 13 tests covering authoritative snapshots, local receive time, monitor recovery, duplicate/regression handling, stale retention, capped/reset backoff, search/navigation keys, discovery degradation, startup retry, cancellation of open/delay/current stream, awaited close, bounded nonblocking input, shutdown, render failure, and cleanup-error reporting.
- [x] GREEN (terminal) — `cargo test terminal::tests -- --nocapture` passed 8 tests covering staged acquisition rollback, every restoration failure, continued later cleanup, construction/render failure, panic owner-thread behavior, previous-hook restoration, failure reporting, and exactly-once cleanup.
- [x] Production binary — `src/main.rs` parses the audited CLI, constructs the Harold source and tmux navigator, and calls the synchronous runtime; `cargo run -- --wat` selected `target/debug/tmx-agent-dash` and returned the expected sanitized CLI error with exit 1.
- [x] RED (runtime review) — independent review found discarded terminal resize events, frozen ages on quiet streams, and absent endpoint feedback; the initial regression suite failed to compile because typed redraw/cadence/endpoint APIs did not exist. The review's runtime-build ordering finding referred to a transient earlier snapshot; stable code already built Tokio and installed signals before terminal acquisition.
- [x] GREEN (runtime review) — typed resize redraw plus a coalesced 100ms cadence refresh every runtime phase without mutating App; an injected clock recorded quiet-stream draws at 100 and 200; `Options.endpoint` is sanitized/capped to 512 scalars before typed retry status/UI rendering, including hostile control-sequence coverage. Runtime tests now pass 17.
- [x] Full Task 7 gate — `cargo test --all-targets` passed 103 library tests, 0 binary tests, and 1 example; `cargo fmt --package tmx-agent-dash -- --check`, warnings-denied Clippy, `cargo build`, and `git diff --check` passed. No dependency or Cargo manifest change was made.
- [x] Final Task 7 independent re-review — no substantive findings; explicit thumbs-up after fresh 17 runtime tests, 8 terminal tests, 103 library tests plus 1 example, formatting, warnings-denied Clippy, build, and diff checks. Task 6's borrowed/build-once detail path remains intact.
- [x] LIVE HANDOFF — launched `cargo run` in designated pane `%26`; ST-010, ST-017 through ST-021, and ST-024 record the actual screen, keyboard, signal, terminal-restoration, and isolated-client evidence. The remaining injected PTY failure cases are tracked separately under Verification.

### Task 8: Documentation and retained HTML references

- [x] Added `README.md` with installation prerequisites, endpoint selection, complete key/search behavior, transport and monitor state semantics, stale retention, navigation limits, terminal shutdown, security boundaries, and troubleshooting.
- [x] Preserved the exact implemented missing-summary copy `No work summary reported` and documented that local search continues while snapshots arrive.
- [x] Rendered and visually inspected retained HTML references at 1440 by 1000 and 760 by 1100 with installed headless Chrome; exact commands, hashes, observations, and limitations are ST-011 and ST-012 in `screen-testing.md`.
- [x] Fresh non-live Task 8 gate — `cargo fmt --all -- --check`, `cargo test --all-targets` (103 library tests, 0 binary tests, and 1 example), `cargo clippy --all-targets --all-features -- -D warnings`, `cargo build --release`, and `git diff --check` passed.
- [x] Independent Task 8 Steps 1/3 review verified README claims against `dc7c491`, inspected both PNGs and hashes, checked ledger ordering and staging state, and gave an explicit thumbs-up with no findings after the monitor/row-state documentation correction.
- [ ] Live Harold/provider integration remains coordinator-owned: real Codex and OpenCode lifecycle evidence passes in ST-025, reviewed two-provider concurrency with current busy summaries passes in ST-028, and final reviewed durable placeholder repair/replay passes in superseding ST-031. Claude authentication and three-provider concurrency remain open.

### Approved status-colour and q-only input slice

- [x] RED (status semantics) — `cargo test ui::tests::table_and_selected_detail_keep_state_words_with_semantic_glyph_colors -- --nocapture` exited 101 because the renderer produced zero `● IDLE` labels instead of the required table/detail pair.
- [x] GREEN (status semantics) — the focused regression passed with two occurrences each of green `● IDLE`, amber `● BUSY`, and muted-grey `○ UNKNOWN`; every non-space glyph/word cell had the asserted Ratatui foreground.
- [x] RED/GREEN (compact status word) — the selected compact `○ UNKNOWN` fixture failed because the 10-cell state column clipped the required word; increasing that existing column to 11 cells preserved `▶ ○ UNKNOWN` and its muted foreground without removing another column.
- [x] RED (q-only input) — focused App and runtime tests returned `Quit` for outside-search Esc instead of `None`/`Continue`; the footer test could not find `q quit`; and the example still accepted Esc as quit. All four commands exited 101 for the intended missing behavior.
- [x] RED (revised accepted-filter contract) — after accepting a non-empty query with `Enter`, the focused App and runtime regressions exited 101 because outside-edit `Esc` left that active query in place; the Ratatui hint regression also exited 101 until `Esc clear` was restored.
- [x] RED/GREEN (empty edit query, final ruling) — the focused App and runtime assertions each exited 101 because edit-mode `Esc` left editing active when its query was empty; restoring the unconditional edit-mode clear/exit branch made both pass, while the separate non-editing guard preserves accepted-filter clear and no-filter no-op behavior.
- [x] GREEN (q-only input) — focused App tests prove Esc clears and exits editing, clears accepted non-empty queries, leaves non-editing empty state unchanged, and always keeps the TUI running; edit-mode `q` remains query text and outside-search `q` quits. The runtime, Ratatui footer, and example focused tests also pass.
- [x] Browser evidence — ST-022 and ST-023 retain and inspect the final 1440 by 1000 and 760 by 1100 HTML references. The first compact render exposed footer clipping; token-only compact spacing fixed it before the retained pass.
- [x] Fresh automated gate — App 17 passed, runtime 17 passed, UI 34 passed, full `cargo test --all-targets` passed 106 library tests plus 1 example, `cargo fmt --all -- --check`, warnings-denied Clippy, debug and release builds, and `git diff --check` passed.
- [x] Independent completion review — fresh review of the final three-state Esc contract, q-only quit behavior, semantic status colours, docs, screenshots, ledger ordering, dependency scope, and all gates found no issues and returned an explicit thumbs-up.
- [x] Coordinator live gate — ST-024 proves edit-mode Esc always clears and exits (including an empty edit buffer), outside-edit Esc clears an accepted filter or leaves the no-filter state unchanged, q-only quit/restoration, and all three semantic live state treatments in `%26`; the reviewed dashboard was relaunched and left running.
