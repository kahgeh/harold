# Portable install implementation

Spec: [spec.md](spec.md). User requested creation of install/reinstall tooling for
another Mac; proceed through implementation and review. Existing fresh-install
and architecture edits are preserved.

- [x] Inspect deploy, config, hooks, dashboard, prerequisites, and local launchctl manuals.
- [x] Define one managed install layout and failure behavior before implementation.
- [x] Add read-only `--check-config` and bounded `--check-ready` CLI with tests.
- [x] Implement installer and service control with prerequisite checks, config
  prompts/import, staged builds/signing, consistent environment, fresh-state
  reinstall archives, and exact-owner readiness.
- [x] Route shared-hook startup through the service owner; test failure/start races.
- [x] Replace duplicated Makefile deployment and document new-Mac setup/reinstall.
- [x] Run focused tests, existing affected suites, formatting, Clippy, and link checks.
- [x] Exercise real isolated install, normal reinstall preserving state, fresh
  reinstall, owner/readiness checks, and cleanup with production untouched.
- [x] Obtain completion reviewer approval and record results.

## Results

- Explicit user choice: manual/on-demand service only. The plist remains in the
  installed bundle, with no login registration. Installation starts once to verify.
- `cargo test --offline --locked --workspace`: 381 passed, 0 failed, 2 existing
  live-provider/tmux tests ignored. Includes six actual-binary probe integrations.
- Python standard-library tests: 13 installer/service and 7 shared-hook tests passed.
  Hook tests run as `python3 -m unittest hooks.shared.harold_turn_complete_test`.
- `cargo clippy --offline --locked --workspace --all-targets -- -D warnings`,
  `cargo fmt --all --check`, Bash syntax, Make target dry-runs, and diff checks passed.
- All 47 local links and heading anchors in live documentation resolve.
- Real isolated installation and reinstall acceptance passed; see
  [live evidence](live-acceptance.md). This included poisoned parent environment,
  spaces/apostrophe in prefix, preserved database inode, fresh archived state,
  changed endpoint, actual hook startup, unrelated listener protection, and cleanup.
- Review findings fixed: probe errors now redact potentially secret configuration
  values; installer selects exact Cargo-reported artifacts for custom target layouts.
  Both have regression tests and the fixed release passed the final live rerun.
- Completion reviewer approved with no remaining findings.
- No dependencies, production installation, or provider configuration were modified.
  All source changes remain local and uncommitted. Notification/channel acceptance
  still requires the explicit setup steps in the guide.
