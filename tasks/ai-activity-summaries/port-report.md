# Task 1: Claude generation port

Implemented in `harold/src/activity_summary.rs`, `harold/src/activity_summary_tests.rs`, `harold/src/settings.rs`, `harold/config/default.toml`, and `harold/config/local.template.toml`. No dependency changes or downloads; no commits. Root module declaration and runtime wiring belong to the integration worker.

## Behavior

The production port sanitizes and bounds evidence itself, sends a JSON evidence object over stdin, and explicitly distinguishes a requested task from a reported outcome. Neither input nor evidence structs implement Debug. A fixed system instruction treats evidence as untrusted data and preserves uncertainty and pending tests. Claude runs with safe mode, no tools, no session persistence, and an isolated directory with mode 0700. The installed Claude help was checked for these flags.

Stdout reading, stdin writing, and process waiting run concurrently under a single timeout. A byte cap stops excessive output even when stdin is blocked. Stderr is discarded; errors expose only bounded enum codes. Only a successful exit plus a decoded `is_error = false` result envelope can yield a description; normalization removes terminal controls and caps output at 160 Unicode scalars. Empty and known placeholder descriptions are rejected.

Success, timeout, and failure explicitly reap the process before removing its directory. Cancellation synchronously requests termination, then transfers the child and directory to an async reaper. Tokio kill-on-drop is an additional fallback. Cancellation and forced-abort tests prove the process is gone and its request directory removed before provider shutdown returns.

Typed settings capture only HOME, USER, LOGNAME, PATH, TMPDIR, LANG, LC_ALL, SSL_CERT_FILE, SSL_CERT_DIR, ANTHROPIC_API_KEY, and CLAUDE_CODE_OAUTH_TOKEN. Captured values are redacted from Debug and cannot be populated through serde. The process starts with an empty environment and receives this allowlist. Existing [ai] values are unchanged.

## Validation bounds

- timeout_ms: 1..=120000
- max_concurrent: 1..=16
- max_pending: 1..=4096
- max_instruction_chars and max_reply_chars: 1..=32000 each
- max_output_bytes: 1..=1048576 (default 65536)
- cli_path: nonempty, no controls, at most 4096 bytes
- model: nonempty ASCII token, at most 128 bytes, starts alphanumeric, remaining characters alphanumeric or -._:/
- effort: low, medium, high, xhigh, or max (installed CLI supported values)

## Red/green evidence

1. Initial `cargo test --offline -p harold activity_summary` runs were blocked by concurrent integration-worker initializer errors. The worker fixed them; those compilation failures were not counted as TDD evidence.
2. Intended RED: `cargo test --offline -p harold activity_summary` against the port/validator skeleton returned exit 101: 4 passed, 9 failed, 1 ignored. Failures included successful-result decoding, CLI isolation/evidence, invalid settings/input, nonzero exit, stdout overflow, timeout, and cancellation; the port stub returned InvalidOutput and the validator returned zero errors.
3. First implementation run exposed two fake-process fixture issues under concurrent interpreter startup: the timeout could precede the Python marker, and the raw single os.write did not reliably complete the overflow fixture. The fixture now uses buffered write plus flush and allows 1500ms for the deliberately hanging process. The production timeout/read concurrency logic was unchanged for these fixture fixes.
4. GREEN: `cargo test --offline -p harold activity_summary::tests` returned exit 0: 10 passed, 0 failed, 1 ignored, 1.61s. All ordinary tests use local fake executables and never invoke a real provider.
5. `rustfmt --edition 2024 harold/src/activity_summary.rs harold/src/activity_summary_tests.rs harold/src/settings.rs` completed successfully. Scoped `git diff --check` passed.

The ignored production probe is intentionally separate: `cargo test --offline -p harold activity_summary_real_claude_probe -- --ignored --nocapture`. It uses the actual Claude port with captured authentication, Sonnet/low, a 60-second timeout, and synthetic fictional retry-loop evidence with tests explicitly pending. The coordinator owns running and recording that live acceptance check, full workspace gates, and final completion review.

## Shutdown and review delta

A scoped review approved the original port with one minor test gap: the backward-compatible configuration test previously loaded only the updated defaults. It now also deserializes a complete legacy settings fixture that omits `activity_summary` entirely.

The coordinator identified a shutdown ownership gap: aborting a provider job could return before its detached reaper finished. Added an `ActivitySummarizer::shutdown` default method and a Claude implementation that drains outstanding cancellation reapers. The initial reaper-only counter was superseded by a mutex-protected admission gate and Notify. A request token registers before evidence preparation and subprocess spawn, stays owned throughout the active RequestProcess lifetime, and transfers without a decrement/increment gap into cancellation reaping. Shutdown closes admission before inspecting the active count, then waits for every token to be released after child reaping and directory removal. This covers an aborted request whose future has not yet been polled for cancellation and prevents new requests starting after shutdown observes zero. No task handles accumulate. The runtime owner aborts/joins provider jobs before awaiting this drain, retains an outer provider owner for forced monitor shutdown, and gives the provider's own timeout a one-second grace before applying the scheduler deadline.

RED: `cargo test --offline -p harold activity_summary::tests::shutdown_waits_for_cancellation_reaping_and_directory_cleanup -- --exact` returned exit 101, 1 failed, asserting the request directory still existed after the no-op shutdown. GREEN after implementation: `cargo test --offline -p harold activity_summary::tests` returned exit 0, 11 passed, 0 failed, 1 ignored, 1.51s. This test cancels the request directly before the reaper can run, then requires shutdown to return only after both process and directory are gone.

The local configuration template now uses the portable `/path/to/claude`; the concrete user executable remains in the synthetic live-probe test and coordinator documentation. A reported Clippy collapsible-if in the test marker polling helper was simplified to a let chain.

Package Clippy after that test-helper fix: `cargo clippy --offline -p harold --all-targets -- -D warnings` returned exit 0 in 2.57s.


## Final forced-abort correction and scoped review

RED: `cargo test --offline -p harold activity_summary::tests::shutdown_` initially returned exit 101 with 1 passed and 2 failed. The new cases caught directory persistence when shutdown ran before an aborted request was joined, and acceptance of new work after shutdown. Both pass with the admission gate and request-lifetime token described above.

Short fake-provider deadlines (1500ms intentional timeout and 2000ms ordinary requests) remained intermittent during concurrent process startup. The isolated overflow regression passed in 0.14s. The fixture now resolves the native Python interpreter once through the existing /usr/bin/python3, gives ordinary fake requests 10 seconds, and gives the intentional hanging-stdin case 8 seconds. Production deadlines remain unchanged. Final focused verification: `cargo test --offline -p harold activity_summary::tests` returned exit 0, 13 passed, 0 failed, 1 ignored, 8.03s.

The scoped completion reviewer independently ran all three shutdown regressions and the truly omitted-section configuration test, reviewed the final fixture delta, and gave final Task 1 approval with no code findings. Full workspace verification and deployment remain coordinator-owned gates.
