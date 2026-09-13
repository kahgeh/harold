# macOS ARM64 GitHub Actions builds

Scope: build and test the locked workspace on a native Apple Silicon runner, then
upload both release binaries as a checksummed archive. Linux is explicitly deferred.
Runs only when a release is published and attaches assets to that existing release.
No installer changes or login startup registration.

- [x] Check native runner labels, build prerequisites, and existing tests.
- [x] Add commit-pinned Actions workflow for published releases only.
- [x] Document release asset contents and the source-installer boundary.
- [x] Validate workflow syntax and local tests/package smoke checks.
- [x] Obtain completion review and address findings.

## Review and validation

- Ruby YAML parse and `git diff --check` passed.
- `cargo test --offline --locked --workspace`: 381 passed, 2 ignored.
- Installer and hook Python tests: 20 passed.
- Executed the workflow packaging shell against existing local ARM64 release
  binaries in a temporary directory: architecture validation, ad-hoc signature
  verification, `harold --help`, archive contents, and checksum all passed.
  This caught and corrected `lipo` argument order before review completion.
- Hosted execution (including the explicit target build on macOS 15) remains
  unverified until the workflow is pushed to GitHub.
- Completion reviewer approved with no findings.

## Release-only follow-up

- [x] Replace push/PR/manual triggers with `release: published`.
- [x] Upload archive and checksum to the triggering release using GitHub CLI.
- [x] Grant contents write permission; scope token to the upload step.
- [x] Update README and lessons for the corrected distribution requirement.
- [x] Validate release trigger/upload command and obtain completion review.

The previous Rust and packaging evidence still applies; build steps are unchanged.
No release has been published or modified during local validation.

Release-only validation: exact YAML trigger/permission assertions, shell syntax,
mocked CLI upload argument check, and diff whitespace check passed. Completion
reviewer approved with no findings. Hosted release upload remains unverified.
