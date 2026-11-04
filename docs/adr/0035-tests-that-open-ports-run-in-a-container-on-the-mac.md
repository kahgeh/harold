# 35. Tests that open ports run in a container on the Mac

Date: 2026-11-05
Status: Accepted.

## Context

Some tests open a port or start a second Harold. On the Mac they could reach the installed Harold. Their ignore text said that CI ran them. CI did not run them.

## Decision

1. `scripts/test-run --container <set>` runs a container test set before the merge. It uses a podman container with an `--internal` network, no capabilities, a read-only root, a read-only source and crate seed, and no Mac environment. The image has a base named by digest. The chief audits the image.
2. `--internal` is the isolation. The `apt-get` of the image build is the one network use. It follows the audit.

## Consequences

These tests run before the merge and cannot reach the installed Harold. CI also runs them on x86_64.

A container image is a supply-chain source. It has its own audit row.

The test logs follow ADR 0029. The runner copies result files out through its secret scan.
