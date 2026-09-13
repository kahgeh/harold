# Runtime recovery implementation evidence

The runtime now owns an ephemeral fingerprint checkpoint for each full tracked incarnation. Inventory discovery, hook resolution, completion resolution, and replay seeding all establish a baseline immediately. Departure and replacement discard it. Baselines adopt no instruction, including the first successful capture after failures.

Recovery triggers use raw visible state before hook-grace filtering: every Busy edge, one Idle retry when Busy recovery was inconclusive or failed, and a 30-second cadence during sustained Busy. Failed initial captures retry at that cadence even when Idle; established Idle panes do not poll history. Recovery failures preserve screen degradation until successful recovery. Checkpoint alignment uses a linear prefix-function algorithm and stores only ordered 32-byte fingerprints.

A checkpoint that discovers a candidate remains tentative until the source event appends. The existing event classifier value `tmux-submitted-prompt-v1` distinguishes proven new submitted occurrences from historical generic visible observations. This preserves legacy duplicate-screen event behavior while allowing identical repeated submissions to refresh source recency and invalidate stale Sonnet generations. No event/protobuf/storage schema changed for this task.

## Test evidence

- RED: `proven_repeated_submission_refreshes_recency_and_generation_basis` retained `Another task` rather than recovering the identical repeated `Fix the retry loop`.
- RED: checkpoint overlap test emitted no new candidate for an additional identical fingerprint occurrence.
- RED: `submitted_prompt_recovery_baselines_then_recovers_new_busy_work_inside_hook_grace` observed zero history captures on discovery rather than one.
- GREEN: ten runtime recovery sequence tests covering baseline discovery paths, hook-grace independence, Busy-to-Idle retry, capture failure, append failure, repeated text, replacement, departure/rejoin, seeded restart, and recovered source feeding Sonnet while rejecting an older result.
- GREEN: checkpoint tests cover exact 29/30-second boundaries, Idle failure retries, sustained Busy with inconclusive state, ineligible anchor blocks, empty baseline, sliding overlap, lost overlap, repeated identical prompts, and exhaustive small-sequence comparison against a simple suffix/prefix oracle.
- `cargo test --offline -p harold agent:: -- --nocapture`: 128 passed, 1 ignored after the final review fixes.
- `cargo clippy --offline -p harold --all-targets -- -D warnings`: passed.
- Scoped formatting and `git diff --check`: passed.

The existing raw-capture privacy integration fixture now supplies separate baseline, visible, and recovery captures, and still checks events, gRPC, snapshots, persisted files, and logs for unrelated text leakage.

The pre-existing forced-summary-shutdown readiness fixture had a parent-observed parallel-load startup timeout. Its process-ready allowance was increased from 2 to 10 seconds; the actual forced-shutdown cleanup assertion remains 2 seconds. This is readiness headroom, not relaxed cleanup verification.

Live tmux/dashboard acceptance and whole-change completion review remain parent-owned gates.

## Completion review fixes

The reviewer found a candidate append failure at Busy followed immediately by Idle could lose retry eligibility. A new runtime test reproduced the missing task (RED), and now passes (GREEN). Failed source persistence records raw visible state and keeps a pending retry without consuming the fingerprint checkpoint. Busy-to-Idle retries once immediately; a failed Idle retry remains eligible after 30 seconds. A deterministic checkpoint test verifies 29/30-second behavior and successful retry of the same candidate.

A parallel agent-suite run exposed an intermittent missed history capture. Boundary inspection found the acquisition thread notified its receiver before dropping its semaphore permit, so an immediate subsequent capture could observe a busy gate after a successful completion. Completion now explicitly releases the permit before notifying the receiver. A 1,000-acquisition gate-release regression passes; it also passed before the fix, so it is stress coverage rather than a deterministic reproduction of the timing race. The final full focused agent suite passed 128 tests with one ignored live probe.
