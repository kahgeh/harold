# Latest Events Crate Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Pull the latest `events` crate into Harold and preserve its turn-completion notification and inbound-routing behavior.

**Architecture:** Use one refreshed `EventStream` for durable ordered facts. Keep Harold's handler cursor and delivery outbox in a separate Turso application database so staging and cursor advancement commit atomically.

**Tech Stack:** Rust 2024, Tokio, Tonic, Turso 0.5.1, `events` at `a23c70c`.

**Spec:** `tasks/update-events-crate/spec.md`

## Global Constraints

- Preserve all unrelated dirty-tree changes.
- Do not modify or delete the old-format runtime event files.
- Preserve existing notification and inbound-routing behavior.
- Follow test-driven development: observe each new behavior test fail before production implementation.
- Do not build until the approved root lockfile remediations are applied and audited.
- Keep retention policy deferred.

---

### Task 1: Advance and safely resolve the dependency graph

**Files:**
- Modify: `events` gitlink
- Modify: `Cargo.toml`
- Modify: `harold/Cargo.toml`
- Modify: `Cargo.lock`

**Interfaces:**
- Consumes: `events` commit `a23c70c13588beeb9ebd4a248d4b91f5bad8bd46`
- Produces: workspace-visible `events::EventStream` API and direct `turso 0.5.1` access for Harold state

- [x] Confirm `events` is checked out at the approved commit and the parent records the changed gitlink.
- [x] Change the workspace Turso dependency from exact `0.4.4` to exact `0.5.1` and add `turso.workspace = true` to Harold.
- [x] Resolve the root lockfile to the approved patched versions: `anyhow 1.0.103`, `crossbeam-epoch 0.9.20`, `rand 0.8.6`, and `rand 0.9.3`.
- [x] Run `cargo audit --no-fetch`; confirm the security scan passes with only the explicitly accepted unmaintained `paste` warning.

### Task 2: Define the refreshed store contract with failing tests

**Files:**
- Create: `harold/src/store_tests.rs`
- Modify: `harold/src/store.rs`

**Interfaces:**
- Consumes: `events::EventNamespaces`, `EventStream`, `EventStreamVersion`, `EventEnvelope`
- Produces: `HaroldStore::open`, `append_turn_completed`, `append_inbound_message`, `stage_unhandled_events`, `next_pending_delivery`, `mark_delivered`, and `record_delivery_failure`

- [x] Write a real temporary-directory test that opens `HaroldStore`, appends a `TurnCompleted`, and reads it back from the `harold/main` stream.
- [x] Run the focused test and confirm it fails because `HaroldStore` does not exist.
- [x] Implement `HaroldStore::open` through `EventNamespaces`, including the checksum-tracked application-state migrations.
- [x] Implement both append helpers with `ExpectedVersion::Any`, `workflow_kind: None`, and `WorkflowRef::None`.
- [x] Run the focused append/read test and confirm it passes.
- [x] Write a test that appends both event types, stages them, and asserts a final-version checkpoint plus two ordered pending deliveries.
- [x] Mutation-check ordering by reversing the pending query and confirming the ordering test fails, then restore it.
- [x] Implement atomic outbox staging and checkpoint advancement in one `BEGIN IMMEDIATE` transaction.
- [x] Run the staging test and confirm it passes.
- [x] Write and pass tests proving a second staging pass creates no duplicate outbox rows and a recorded delivery failure remains pending until `mark_delivered` succeeds.

### Task 3: Replace the old projector while preserving dispatch behavior

**Files:**
- Modify: `harold/src/projector.rs`
- Modify: `harold/src/inbound/mod.rs`
- Create: `harold/src/projector_tests.rs`

**Interfaces:**
- Consumes: pending delivery rows from `HaroldStore`
- Produces: serial handler loop dispatching `TurnCompleted` to `notify` and `InboundMessageReceived` to `route_inbound_message`

- [x] Write a focused test showing one handler cycle stages and dispatches the two event kinds in version order through a recording dispatcher.
- [x] Run it and confirm it fails against the removed crate-owned projector design.
- [x] Introduce a small dispatcher boundary and implement the production dispatcher using the existing blocking notification/routing functions.
- [x] Remove the unused event-store argument from `route_inbound_message` and update its callers.
- [x] Implement polling, staging, ordered delivery, failure recording, poison-event handling, and shutdown observation without recreating the old `events::Projector`.
- [x] Run the focused handler tests and confirm they pass.
- [x] Add and pass restart, retry, poison-event, and idle-shutdown tests.

### Task 4: Wire the refreshed store through Harold

**Files:**
- Modify: `harold/src/main.rs`
- Modify: `harold/src/channels/mod.rs`
- Modify: `harold/src/channels/imessage.rs`
- Modify: `harold/src/channels/telegram.rs`

**Interfaces:**
- Consumes: `Arc<HaroldStore>`
- Produces: unchanged gRPC acceptance, listener append, and concurrent handler startup behavior

- [x] Update service, listener, and handler signatures from `EventStore` to `HaroldStore`.
- [x] Remove the obsolete shutdown WAL-checkpoint call.
- [x] Add a service-level test that calls `turn_complete`, observes `accepted: true`, and reads back every request field from the stream.
- [x] Run the service acceptance test and confirm it passes through the refreshed stream.
- [x] Run the full Harold test suite and resolve regressions without modifying unrelated dirty files.

### Task 5: Update live documentation and complete verification

**Files:**
- Modify: `docs/explanations/architecture.md`
- Modify: `docs/references/operation/README.md`
- Modify: `docs/references/notification/README.md`
- Modify: `docs/references/inbound-message-routing/README.md` only around the store/handler terminology, preserving current unrelated edits
- Modify: `tasks/update-events-crate/todo.md`

**Interfaces:**
- Consumes: verified implementation behavior
- Produces: documentation describing `EventStream`, Harold-owned handler state, outbox delivery, and shutdown accurately

- [x] Replace stale crate-owned projector/checkpoint descriptions with the implemented handler/outbox flow.
- [x] Run `cargo fmt --all -- --check`.
- [x] Run `cargo clippy --workspace --all-targets --offline -- -D warnings`.
- [x] Run `cargo test --workspace --offline`.
- [x] Run `cargo doc --workspace --no-deps --offline`.
- [x] Run `cargo audit --no-fetch` and reconcile results with the approved exceptions.
- [x] Run `git diff --check` and inspect the complete diff plus submodule pointer.
- [x] Record commands and outcomes in this file's Review section.
- [x] Request independent completion review, address every finding, and repeat verification.

### Task 6: Deploy and verify the running daemon

- [x] Diagnose the initial post-build exit and fix `make deploy` to launch from the installed config directory with an absolute argv that the next deploy can stop.
- [x] Build the release artifact through the documented `make deploy` workflow.
- [x] Preserve a rollback copy of the currently installed binary before replacement.
- [x] Confirm the old daemon exits and a new process starts on `127.0.0.1:50060`.
- [x] Exercise the live `TurnComplete` gRPC endpoint and confirm `accepted: true`.
- [x] Confirm the deployed handler stages and completes the test event from the new `harold/main` stream.
- [x] Re-run the completion reviewer after deployment evidence is recorded.

## Review

- `events` is pinned at `a23c70c13588beeb9ebd4a248d4b91f5bad8bd46`; the parent gitlink advances from `d50236e`.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets --offline -- -D warnings`: passed.
- `cargo test --workspace --offline`: passed (52 `events` tests and 37 Harold tests).
- `cargo doc --workspace --no-deps --offline`: passed.
- `cargo audit --no-fetch`: no vulnerabilities; one accepted unmaintained warning for transitive `paste 1.0.15` through Turso.
- `git diff --check`: passed.
- Mutation checks proved the ordering, delivered-marker, and migration-checksum tests fail under their corresponding regressions and pass after restoration.
- Initial completion review found production delivery failures were being erased, malformed known payloads lacked coverage, and shutdown wording was too strong. The effect APIs now return typed outcomes, real TTS/HTTP/process/tmux failures remain pending, both poison cases are covered, and the shutdown reference matches actual sequencing.
- Second completion review found Telegram request URLs could leak the bot token into durable errors, a partial iMessage send could skip its missing question on retry, and the shutdown diagram retained the old ordering. Request URLs are now stripped, response bodies bounded, part-aware iMessage retry tests pass, and the sequence is corrected.
- Third completion review found a matching generic question from an unrelated turn could be mistaken for completion. Retry inference now requires the current question and main notification pair; a collision regression test passes.
- Independent completion re-review: approved with explicit thumbs-up and no remaining findings.
- Release deployment built and signed successfully. Rollback binary: `~/bin/harold/harold.pre-events-a23c70c`.
- The initial launch exposed a pre-existing `make deploy` working-directory bug; the target now starts Harold from `~/bin/harold`, where `./config` resolves correctly.
- Final redeployment replaced PID `76188` with PID `80498`, started at `2026-08-23 17:43:30 +1000`; it has PPID 1, uses the absolute installed-binary argv, and listens on `127.0.0.1:50060`.
- Live `TurnComplete` returned `accepted: true`. A read-only state snapshot showed `TurnCompleted` version 1, `attempt_count = 0`, no error, `delivered_at_ms` set, and checkpoint `harold/main = 1`.
- After correcting stop/start argv matching and redeploying, a second live RPC returned `accepted: true`; its outbox row is version 2 with zero attempts, no error, and delivered state, while the checkpoint advanced to 2.
- Existing old-format root event files remain present; the refreshed stream was created under `~/bin/harold/data/events/harold/main/`.
- Deployment completion review: approved with explicit thumbs-up and no remaining findings.
