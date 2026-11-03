# 33. A release trusts the checks of each push

Date: 2026-11-04
Status: Accepted. Supersedes the gate of ADR 0026.

## Context

A release took about an hour. The same tests ran on the push, on the Mac and in the release run. Each run compiled from nothing.

## Decision

The Linux and macOS checks run on each push to `main`, with a cache of dependencies. `make release` publishes only a commit that equals `origin/main` and whose two push checks ended in `success`. It runs only the timing tests on the Mac. The release run only builds, with no cache.

## Consequences

A release takes about 15 minutes after the checks pass. The release run does not test the commit again, so trust rests on the trust rule.

The macOS checks are free because the repository is public. A private repository bills macOS minutes at the higher rate.

A test build may use a cache. A release build never does.

Each push-check job restores the cache before the build and saves it after, with `if: always()` on the save. A step that fails or times out still saves the work it did, so the next run starts closer to done. The `Test` step has a limit of 40 minutes. The `Check the feature mixes` step has a limit of 30 minutes.
