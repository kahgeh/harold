# Release bootstrap installer

Install the latest published macOS ARM64 release through curl piped to sh.
Reuse the existing installer for configuration, safe replacement, and manual service
control. Package installer code with binaries so both come from the same release.
Require Python 3.9+, tmux and grpcurl; do not install system dependencies implicitly.

- [x] Add prebuilt input to existing installer with regression coverage.
- [x] Package installer/service/hook with release binaries.
- [x] Add POSIX shell bootstrap: platform gate, HTTPS downloads, checksum validation,
      temporary cleanup, terminal-aware wizard and argument forwarding.
- [x] Document exact command and prerequisites.
- [x] Exercise bootstrap with isolated fake downloads; review completion.

## Validation

- 19 installer/bootstrap tests and 7 hook tests pass.
- Piped-shell fixture verifies download/checksum/argument forwarding; corrupt
  checksums and archive traversal are rejected before installer execution.
- Workflow package step executed with local release binaries: ARM64 and signing
  validation, binary help, archive checksum, and packaged installer import pass.
- Shell syntax and diff whitespace checks pass.
- Completion reviewer approved with no findings. Public bootstrap and release assets are now published and verified below.

## Published-release acceptance

- [x] Commit and push bootstrap changes.
- [x] Publish first release v0.1.0 and verify hosted build succeeds.
- [x] Run public curl bootstrap into isolated prefix with synthetic config.
- [x] Verify installed process, readiness, manual control; clean test service.
- [x] Record exact results and remaining fresh-machine limitation.

The first hosted run (34760572312) failed before compilation because events
was private. The user made events public; anonymous API access is confirmed.
The original recursive submodule checkout now works without additional secrets
or deploy keys. Re-running the same v0.1.0 workflow preserves its tagged source.

## Published acceptance results

- Bootstrap commit: b9051fef66ccf8018db037914c74fc79af6ba473 on origin/main.
- Release: https://github.com/kahgeh/harold/releases/tag/v0.1.0
- Hosted run: https://github.com/kahgeh/harold/actions/runs/34760572312
  Attempt 2 passed all steps in 6m41s, including tests, release compilation,
  ARM64/signature checks, packaging, checksum and asset upload.
- Anonymous public `curl -fsSL .../main/scripts/bootstrap.sh | sh -s --
  --prefix <temporary>/bin --config <synthetic-settings>` exited 0.
- Downloaded archive checksum passed; installed daemon owned its loopback
  listener at 127.0.0.1:59095, PID 10893; readiness returned true.
- Managed store path and absence of automatic login plist verified.
- Manual stop removed readiness; manual start restored readiness with PID 16000.
- Final stop removed launchd job and listener; temporary installation removed.
- Existing production install was untouched; no notification RPC was sent.
- This proves the published download/install/control path on the current Mac.
  A separate fresh-Mac test and interactive credential-wizard acceptance remain
  unperformed; this run used supplied synthetic settings and existing prerequisites.

Final completion reviewer independently confirmed public assets, hosted success,
install log readiness, and cleanup; approved with no findings.
