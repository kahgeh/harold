# Inventory timeout recovery (GitHub #2)

Evidence: local full process scan of ~1188 processes took169–180ms normally,
586–756ms with taskpolicy -b. Production inventory has a hardcoded500ms deadline;
late results are discarded and occupied workers are also reported as timeout.

Fix: separate inventory_timeout_ms default3000 from screen deadline500ms. Expose
configuration, classify worker occupancy as busy, log health transitions without
raw process/screen data. Preserve worker bounds and last known panes.
Do not narrow the process table to pane TTYs: ancestry matching permits descendants
without that controlling terminal and must not silently lose them.

- [x] Add validated setting/default and wire production runtime.
- [x] Separate inventory deadline, worker occupancy, health transition logs.
- [x] Regression tests: slow scan accepted, timeout/busy/recovery, deduplication.
- [x] Full tests/Clippy, real background-priority scan verification, review.
- [x] Document configuration and summarize issue disposition/release boundary.

## Verification

- Settings regression failed with zero timeout before validation, then passed.
- Full Rust workspace:386 passed,0failed,2ignored. Clippy all-targets with
  warnings denied, fmt, and diff checks passed; completion reviewer approved.
- Isolated actual launchd installation with real ps delayed800ms and synthetic
  tmux fixture reproduced timeout/no panes at500ms and warning logging.
  Restart at3000ms discovered the fixture pane and emitted recovery log.
  Temporary service/listener/files removed; production installation untouched.

## Release delivery

- [x] Publish v0.1.2 and verify hosted build/test/upload.
- [x] Exercise same timeout/recovery check with publicly downloaded release.
- [x] Record delivery evidence and reporting-machine limitation.

## Published acceptance

- v0.1.2 is latest: https://github.com/kahgeh/harold/releases/tag/v0.1.2
- Hosted run https://github.com/kahgeh/harold/actions/runs/34965790266
  passed build, tests, signing, package checks, and asset upload at6256019.
- Public curl bootstrap installed downloaded release into a temporary prefix.
  Checksum passed; service ready PID28148 port53740.
- With800ms injected real-ps delay, configured500ms reproduced degraded inventory
  and no panes; warning logged. Restart at3000ms PID31845 recovered health and
  discovered one controlled fixture pane; recovery logged.
- Temporary service/listener/files removed. Production installation untouched.
- Reporting machine's actual workload remains unverified. Issue #2 remains open
  pending that confirmation; no issue comments or notifications were sent.

Final completion reviewer independently confirmed public assets, successful hosted
run and isolated acceptance evidence; approved with no findings.
