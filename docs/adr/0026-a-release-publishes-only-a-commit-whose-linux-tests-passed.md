# 26. A release publishes only a commit whose Linux tests passed

Date: 2026-11-02
Status: Accepted (rex-state-stage1-cleanup).

## Context

The Build workflow runs on a published release, so its Linux job runs after `make release` has made a pre-release. A Linux failure left a pre-release with no Linux package.

## Decision

A Linux test workflow (`.github/workflows/linux-tests.yml`) runs on each push to `main`. `make release` publishes only when that workflow passed on the commit it releases. Otherwise it names the run to wait for and makes nothing. The Build workflow on the release stays as it is.

## Consequences

A release waits for the Linux tests of its commit. A Linux test failure blocks a release before anything is published, never after.
