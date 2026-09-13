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
- Completion reviewer approved with no findings. Public bootstrap URL and release assets require
  these changes to be pushed and a release containing the bundle to be published.

## Published-release acceptance

- [ ] Commit and push bootstrap changes.
- [ ] Publish first release v0.1.0 and verify hosted build succeeds.
- [ ] Run public curl bootstrap into isolated prefix with synthetic config.
- [ ] Verify installed process, readiness, manual control; clean test service.
- [ ] Record exact results and remaining fresh-machine limitation.
