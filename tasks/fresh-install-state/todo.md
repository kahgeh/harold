# Fresh-install state consolidation

## Scope and decision

The user requires fresh installations only and explicitly rejects migration
compatibility clutter. Consolidate Harold's persisted application state and remove
historical placeholder repair. The earlier architecture readability edit remains.
The user accepts impact to the current installation in favor of a clean design.
No deployment or runtime-data reset is needed to implement that design. Do not
change dependencies or the events submodule.

Keep one checksum-verified initial schema, not four incremental schema steps.
Reopening a store created with that schema is supported. An incompatible schema
must fail without modifying its application data; no conversion or reset is added.
The complete current event contract remains the basis for ordinary restart/replay.

Placeholder matching happens when input arrives. Configuration changes do not
retroactively rewrite previously accepted summaries. Keep valid runtime defaults
and hook/RPC semantics. Remove the deprecated agent configuration shape and
deserialization support used solely by historical event payloads.

## Plan

- [x] Locate historical repairs, schema steps, references, tests, and input filters.
- [x] Record the user's fresh-install rule in `tasks/lessons.md` and qualify the old upgrade lesson.
- [x] Consolidate state initialization in `harold/src/store.rs` and a single
  `harold/src/store/migrations/001_initial.sql`, including all current summary fields.
  Replace upgrade tests with fresh schema, reopen, checksum, and conflicting-schema tests.
- [x] Remove `AgentWorkSummaryCandidatesRepaired` and its reason/variant from the
  domain, runtime, reducer, store serialization/dispatch, and tests.
- [x] Simplify `load_startup_agent_snapshot` to catch up and load; keep ordinary
  restart/replay, commit order, current placeholder rejection, and legitimate text.
- [x] Use only named `[[agents]]` configuration and remove the legacy table branch,
  deprecation warning, and acceptance tests. Preserve normal optional defaults and
  legitimate ambiguous named-provider handling.
- [x] Update architecture/reference/setup docs to describe the current installation,
  schema, and event set. Mark superseded historical task requirements where relevant.
- [x] Run focused offline tests, then workspace tests/check/clippy, formatting,
  cached dependency audit if available, link checks, and diff checks.
- [x] Obtain completion reviewer approval and resolve every required finding.

## Verification approach

First change the fresh-schema test to expect a single initial schema and run it
against existing code (expected failure: four old schema records). Preserve tests
of generated-description persistence/replay and placeholder rejection before append.
Replace repair-only tests with current-contract coverage where they also exercise
startup pagination, state/summary independence, or generation invalidation.

All Cargo commands use `--offline --locked`; no new dependency is authorized or
needed. If an unavailable package prevents those commands, stop fetching and report
the blocker. Source verification does not imply live deployment.

## Review and results

- RED: the schema test observed four old schema records instead of `001_initial`;
  the configuration test observed that the deprecated table was still accepted.
- GREEN: focused store 19/19, settings 9/9, agent 124 passed (one existing ignored),
  startup 2/2, and projector 9/9.
- `cargo test --offline --locked --workspace`: 375 passed, 0 failed, 2 ignored.
  The existing ignored tests require an authenticated Claude probe and a live tmux
  capture respectively; neither is required by this source consolidation.
- `cargo check --offline --locked --workspace` and
  `cargo clippy --offline --locked --workspace --all-targets -- -D warnings` passed.
- `cargo fmt --all --check` and `git diff --check` passed.
- `cargo audit --no-fetch` exited 0 using the cached advisory database, with the
  existing allowed `paste` unmaintained warning. No dependencies changed.
- All 24 local links and heading anchors across five changed live docs resolve.
- Retired repair event, schema-step, and legacy configuration symbols are absent
  from application source and live docs. Historical task requirements are marked
  superseded. Current input filters and normal restart/replay remain covered.
- One complete initial schema replaces the previous SQL steps. Conflicting schemas
  fail transactionally without conversion or deletion. The current installation
  is allowed to be incompatible; no deployment or runtime data operation was run.
- Independent completion review: source and documentation have no findings;
  final approval confirmed after all verification results.
