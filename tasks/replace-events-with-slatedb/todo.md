# Replace `events` with SlateDB

## Planning

- [x] Map Harold's current event-store and projector guarantees.
- [x] Verify current SlateDB APIs, durability model, and object-store requirements.
- [x] Re-audit the refreshed `/Users/kahgeh/Dev/p/events` implementation.
- [x] Compare refreshed `events` guarantees with native SlateDB primitives.
- [ ] Clarify durability, processing, migration, and deployment constraints.
- [ ] Compare replacement approaches and select one with the user.
- [ ] Present and obtain approval for the complete design.
- [ ] Write and self-review the design specification.
- [ ] Obtain user approval of the written specification.
- [ ] Write and self-review the implementation plan using `superpowers:writing-plans`.

## Review

- Harold currently persists two event kinds and runs one projector, but its observed failure behavior is mixed: crashes can replay work while handler-local failures are logged and checkpointed.
- SlateDB can provide durable ordered key/value storage, but Harold must supply queue keys, completion state, retry policy, and effect-level duplicate protection.
- The refreshed `events` crate supplies a well-defined event-stream contract but deliberately leaves projection scheduling, checkpoints, retries, outboxes, and side-effect idempotency to Harold.
- Direct SlateDB would require Harold to recreate event envelopes, logical versions, expected-version checks, immutable key layout, payload compatibility, and any secondary indexes it chooses to retain.
- For Harold's current two-event workflow, most workflow/progress features in `events` are unused; the principal decision is reusable event history versus a purpose-built durable work queue.
- Refreshed `events` revision `a23c70c` replaces the destructive `002_reset_*` migrations with non-dropping `001_*` baseline schemas; migration-focused and full offline tests pass.
- Completed-work retention is deliberately deferred. The design must keep immutable events separate from delivery state so a later retention policy does not require changing the processing model.
- Planning remains in progress. No production code has been changed.
