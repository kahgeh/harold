# Ship Claude and Codex completion hooks

Install both provider adapters next to the shared notifier in Harold's bundle.
Use Python 3.9+ without uv. Preserve user-owned provider settings; document the
commands to register the installed scripts. Package code and installer together.

- [x] Import and test the provider-specific adapters using synthetic transcripts.
- [x] Install and validate both files for source and prebuilt installs.
- [x] Include scripts/tests in release workflow; update setup documentation.
- [ ] Validate packaged hooks from an arbitrary prefix; completion review.
- [ ] Commit/push and publish v0.1.1; verify uploaded contents and public install.

Do not send test notifications or change production provider registrations.

## Local validation

- Python 3.9: 9 provider tests, 19 installer/bootstrap tests, 7 shared-hook tests pass.
- Corrected workflow test invocation to module mode so relative test imports work.
- Executed packaging shell with local release binaries; architecture, signing,
  checksum, and archive content checks pass, including all three hook scripts.
- Provider relocation test confirms shared import follows a custom install prefix.
- Documentation links, command syntax, and diff whitespace checks pass.
