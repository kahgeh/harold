# Activity summary integration report

## Implementation

- `AgentActivitySummaryGenerated` carries full incarnation, actual source event version, bounded generated description, and generation timestamp. It is projection-only and cannot create an outbox delivery.
- The projection retains explicit and screen source candidates separately from its generated candidate. A matching generated candidate wins until the source activity revision changes.
- Revisions advance for every accepted lifecycle/completion, changed substantive screen instruction, true transition to screen Busy, and source repair. Corroborating Busy and metadata preserve the generated description. Runtime revisions are assigned only after successful append, using the returned envelopes by event type rather than batch offsets; the reducer independently verifies eligibility.
- Migration `004_activity_summary_projection` adds checksum-tracked projection columns. Existing migration files and protobuf remain unchanged. Legacy panes start with basis zero and no generated candidate; persisted summaries survive reopening and rebuilding the projection.
- The scheduler keeps at most the configured concurrent requests, one active request per pane, and one latest pending request per pane with a global pending cap. A full queue drops its oldest pending request. Evidence is truncated by Unicode scalar count before queueing. Unchanged screen observations do not queue generation.
- Lifecycle hooks retain the richer source instruction internally before the existing 160-scalar durable display reduction. Completion requests pass bounded original prompt and assistant reply. These inputs are ephemeral scheduler data; no raw screen capture is acquired.
- Production provider wiring is controlled by `[activity_summary].enabled`; existing test constructors stay disabled. Generation runs in spawned tasks, while the monitor continues acknowledging source events. Failed inference/result append leaves source fallback intact.
- Shutdown aborts and joins inference tasks, then awaits the provider cleanup hook. The outer scheduler deadline permits a one-second cleanup grace beyond the provider's own configured request timeout.

## Test evidence

- Initial reducer/store RED attempts were blocked by concurrent module/test-fixture compilation errors. Those attempts did not establish a behavioral RED and are not counted as such.
- A scheduler panic-recovery regression produced a behavioral RED: a provider panic left the pane marked active and its latest pending request never started. Tracking active requests by Tokio task ID and releasing the entry on `JoinError` made the test pass.
- `cargo test --offline -p harold agent::runtime -- --test-threads=1`: 48 passed before the final pending-capacity test was added. The tests cover blocked-provider acknowledgements, identical/new turns, pane replacement/departure, full completion evidence and outcome projection, duplicate screens/Busy corroboration, provider/result-append/source-append failures, concurrency, evidence bounds, cancellation, and panic recovery.
- `cargo test --offline -p harold store::tests -- --test-threads=1`: 19 passed. New coverage includes migration upgrade/checksum validation, generated-event normalization, no-outbox dispatch, failed projection rollback, restart, and full replay.
- `cargo test --offline -p harold generated_activity`: 4 passed before the final Busy-boundary/repair test was added.
- First full package run during provider cleanup development: 175 passed, 1 failed (the port owner's new shutdown cleanup RED), 1 ignored live probe. This is an intermediate result, not the final gate.
- First package Clippy run found only a collapsible `if` in the port test helper; the port owner was notified. Integration code had no reported warning.

- Final worker scope: `cargo test --offline -p harold agent:: -- --test-threads=1`: 94 passed, 0 failed, including all four scheduler tests and the additional Busy-boundary/source-repair invalidation regression. Scoped `git diff --check` passed.

## Remaining integration gates

Coordinator owns the final package/workspace test, formatting, warnings-denied Clippy, release build, isolated live server/provider acceptance, and independent completion review. Deployment remains a separate step. No dependencies were added or downloaded and no commits were created by this worker.


## Completion review fixes

- Reply-only completion evidence: the reviewer found that runtime scheduling required a nonempty instruction despite the provider supporting assistant-only outcomes. A new regression failed waiting for the request. Scheduling now checks the bounded instruction and reply together, skips only when both lack text, and still invalidates the prior activity revision. The regression passes and checks preserved source fallback, reported outcome projection, empty completion invalidation/no request, and exclusion of configured placeholder instructions from both provider input and durable completion events.
- Forced monitor shutdown: `main.rs` now retains a provider owner and calls the shared `stop_agent_monitor` helper after both successful and failed server termination. After its monitor deadline/abort/join, the helper always awaits provider cleanup. The port owner atomically closes admissions and tracks admitted requests through normal or cancellation cleanup, preventing a late request from spawning after shutdown observes no active cleanup.
- A real fake-CLI regression runs inference, blocks the monitor inside a screen acquisition, and invokes that same main helper with a short forced deadline. Before returning, the helper must remove the isolated working directory and reap the child. The blocking acquisition is released only afterward, proving the forced path was exercised. An RAII test guard releases the acquisition on assertion failures as well.
- `cargo test --offline -p harold agent::runtime_tests::activity_summary -- --test-threads=1`: 10 passed, 0 failed after both review fixes. Coordinator retains final broad gates/re-review ownership.

## Parallel fixture startup correction

The coordinator's parallel workspace gate exposed a readiness-marker timeout in the forced-shutdown fixture, before shutdown was invoked. Its `/usr/bin/python3` shebang resolves through the Xcode launcher to `/Applications/Xcode.app/Contents/Developer/usr/bin/python3`, and the separate port tests also observed intermittent Python startup delays under parallel load. The shutdown test needs only its PID, working directory, and a blocked process, so its fixture now uses `/bin/sh` builtins to write those two values and then `exec /bin/sleep 60`, preserving the PID. This removes the unnecessary interpreter/launcher/import startup; the existing readiness timeout was retained. The production implementation is unchanged.

`cargo test --offline -p harold --all-targets`: 181 passed, 0 failed, 1 ignored, in 8.04 seconds after the fixture correction. Output: `/tmp/harold-activity-package-tests.log`. Scoped diff check passed. Coordinator owns rerunning final workspace gates.
