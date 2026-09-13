# Ship Claude and Codex completion hooks

Install both provider adapters next to the shared notifier in Harold's bundle.
Use Python 3.9+ without uv. Preserve user-owned provider settings; document the
commands to register the installed scripts. Package code and installer together.

- [x] Import and test the provider-specific adapters using synthetic transcripts.
- [x] Install and validate both files for source and prebuilt installs.
- [x] Include scripts/tests in release workflow; update setup documentation.
- [x] Validate packaged hooks from an arbitrary prefix; completion review.
- [x] Commit/push and publish v0.1.1; verify uploaded contents and public install.

Do not send test notifications or change production provider registrations.

## Local validation

- Python 3.9: 9 provider tests, 19 installer/bootstrap tests, 7 shared-hook tests pass.
- Corrected workflow test invocation to module mode so relative test imports work.
- Executed packaging shell with local release binaries; architecture, signing,
  checksum, and archive content checks pass, including all three hook scripts.
- Provider relocation test confirms shared import follows a custom install prefix.
- Documentation links, command syntax, and diff whitespace checks pass.

## Published verification

- Commit da22ba5741456b538da1d6e0fcd34debd2297be6 pushed to main.
- v0.1.1 is the latest release: https://github.com/kahgeh/harold/releases/tag/v0.1.1
- All hosted build/test/package/upload steps passed:
  https://github.com/kahgeh/harold/actions/runs/34762176919
- Public curl bootstrap downloaded the archive, verified checksum, and installed
  to a temporary prefix using synthetic settings. Ready PID 81007 at port 64535.
- All three scripts were present. Ran each downloaded provider adapter with
  synthetic transcript input; both produced the expected prompt/reply through
  the shared notifier. A fixture grpcurl intercepted calls; no notification sent.
- Final cleanup removed launchd job, listener, and temporary installation.
- Production installation/provider settings untouched. This is current-Mac
  package/adapter verification, not fresh-machine or live-provider hook acceptance.
- v0.1.0 notes identify its missing adapters and direct users to v0.1.1.

Final reviewer approved published acceptance: 381 hosted Rust and 35 Python tests
passed (two Rust tests ignored), release assets and isolated adapter execution verified.
