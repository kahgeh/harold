# Harold

Stop letting your agents nap. Harold notifies you of idling agents in another room while you are at your desk or even when you are away. When you are away, Harold can direct your replies to your agents, ensuring the work never stops just because you stepped out for a coffee.

The "Stalled Momentum" Problem
At your desk: You’re deep in the zone on Pane 1. Meanwhile, the tasks in Panes 2 and 3 finished five minutes ago and are now just drawing digital breath. Harold breaks the silence by announcing completions via voice, letting you jump in, give the next command, and keep the gears turning without having to manually poll your tabs like a nervous intern.

Away from your desk: A long-running doc-gen or complex fix finishes while you're out. Normally, that agent stays dormant until you get back. Harold pings your iMessage with a concise summary. You reply from your phone, Harold surgically routes the text back to the specific tmux session, and the agent is back to work before you’ve even finished your latte.

## Harold’s Prime Directives

Zero Idle Time: Voice announcements of turn completion.

Remote Command: Use iMessage to feed the beast from anywhere. Review a summary, text a correction, and keep the agent moving.

Contextual Routing: Don't worry about which agent is which. Harold knows exactly where your input belongs, ensuring your workflow stays unbroken across every tmux pane.

## How it works

Harold inventories configured agent processes in tmux, combines explicit lifecycle hooks with provider-specific visible-screen evidence, and stores the resulting current state durably. Existing stop hooks also report completed turns for turn-end updates and non-destructive work-summary updates.

The public agent-state stream starts with a complete current snapshot, then sends later revisions. Raw pane captures remain inside the screen adapter; only classified state and a normalized work summary can cross the acquisition boundary.

See the [architecture explanation](docs/explanations/architecture.md), [agent-monitor reference](docs/references/agent-monitor/README.md), and [hook setup guide](docs/how-tos/setup-agent-monitor-hooks.md).

## Dash

The [`tmx-agent-dash`](tmx-agent-dash/README.md) terminal dash shows Harold's current agent-pane projection and can switch the invoking tmux client to the pane under the cursor.

Install the latest release on an Apple Silicon Mac (macOS 15+):

```sh
curl -fsSL https://raw.githubusercontent.com/kahgeh/harold/main/scripts/bootstrap.sh | sh
```

Requires Python 3.9+, tmux and grpcurl (`brew install python tmux grpcurl`).
The bootstrap verifies the release checksum and opens the configuration wizard.
For a source build, run `./scripts/install.sh` from a checkout.

The installer verifies readiness, and launchd starts Harold again at every login; `haroldctl stop` keeps it off until `haroldctl start`. Use `./scripts/install.sh --reinstall` for fresh storage with retained settings. See [installation and reinstallation](docs/how-tos/setup.md) for another-Mac setup.

Then start the dash inside the tmux client it should navigate:

```sh
tmx-agent-dash
```

## Download release builds

Publishing a GitHub release triggers the **Build macOS ARM64** workflow. After
its tests and build succeed, it attaches `harold-aarch64-apple-darwin.tar.gz`
and its `.sha256` checksum to that release. The archive contains `harold`,
`tmx-agent-dash`, configuration templates, the gRPC schema, the source
revision and the release version. Draft releases, pushes, and pull requests do not trigger builds.
Re-running a release workflow replaces its matching assets.

These builds target Apple Silicon Macs running macOS 15 or newer. Binaries are
ad-hoc signed, not notarized. From v0.1.1, the archive includes the installer, service controller, Claude and Codex completion scripts, and shared notifier. The
bootstrap above installs these together without Rust or a source checkout. Linux
builds are not provided. A published release containing the installer is required.

### Cut a release

Run `make release` in this checkout to publish the pushed `main` as the next release and install it on this Mac. It first checks that the checkout is on `main`, that no tracked file has uncommitted changes, that `main` matches `origin/main`, that `gh` is signed in, that the new tag is free, and that the tests the build workflow runs pass. When a check fails it prints why and publishes nothing.

It then creates the release as a pre-release, which GitHub's latest-release link skips, so the bootstrap on another Mac keeps installing the previous release until this one has a build. It prints each build step as it finishes, marks the release latest when the build succeeds, downloads the archive, checks it against its checksum and runs its installer. The last line names the version, its commit and the running Harold's process ID:

```
Harold v0.6.0 (31323b9) is running, PID 16897.
```

`make install-release` installs a release that already exists, with no checks: the latest one, or the one `VERSION` names. Use it to finish after a failed download, or on another Mac from its own checkout.

Both commands take these options on the `make` command line:

| Option | Effect |
|---|---|
| `VERSION=0.6.1` | Release or install that version. `make release` otherwise raises the middle number of the latest release: `v0.5.0` becomes `v0.6.0`. |
| `NOTES=notes.md` | `make release` only: use that file as the release notes. Otherwise the notes are the subject of every commit since the previous release. |
| `PREFIX=<folder>` | Install under that folder instead of `~/bin`. |
| `SIGNING_IDENTITY="<certificate>"` | Sign with that certificate. Needed once per Mac, on the first install that should use it; the installer saves the name for later installs. |

Every install, by these commands or by `./scripts/install.sh`, moves the previous Harold to `~/bin/harold.backup-<time>` with copies of `haroldctl` and `tmx-agent-dash`, and writes `ROLLBACK.md` there with the commands that put it back. The installer prints that folder. When it prints steps for macOS permissions instead of saying they carry over, follow them; [Keep permissions across installs](docs/how-tos/setup.md#keep-permissions-across-installs) explains why.

## Prerequisites

See [docs/prerequisites.md](docs/prerequisites.md).

## Dependency audits

Always build with `--locked`. Any `Cargo.lock` change goes through the rust-supply-chain-auditor first.

| # | Date | Scope | Crates and versions | Verdict |
|---|------|-------|---------------------|---------|
| 1 | 2025-09-19 | Two per-crate audits in crates-audit, written before Harold used the crates (first used in 4d94049) | askama =0.14.0, datastar =0.3.1. (Harold's pins add features the records do not mention.) | CLEAN, risk LOW. Record: `~/Dev/p/crates-audit/askama-0.14.0.md` and `datastar-0.3.1.md`. |
| 2 | 2026-09-13 | The existing Harold `Cargo.lock` (sha256 `c398be3ae9226843c5bee6ba65c4a90840efcf7c64054f8477766e18505daa8a`) for the release build on the macOS ARM64 GitHub runner | 471 packages: 467 from crates.io and four local. No git dependencies. Direct turso =0.5.1 (resolved core 0.5.3). | CLEAN for building that exact lock with `--locked`. All 467 archive checksums match; cargo audit found no vulnerabilities; paste 1.0.15 is unmaintained (RUSTSEC-2024-0436). Full source review was not done, and any lock change needs a re-audit. Record: `~/Dev/p/crates-audit/harold-locked-release-2026-09-13.md`. |
| 3 | 2026-09-20 | The standalone `examples/opendata-log` tutorial lock (sha256 `b91478108016d4cb0d1e34ac19e903f24b55b3b9d8299429c49b62e30589ca77`), then its dependency extension the same day | 351 crates.io packages. Direct opendata-log 1.0.0, opendata-common 0.1.17, bytes 1.11.1, tokio 1.49.0; the extension adds serde =1.0.229, serde_json =1.0.151 and tokio features `sync`, `time`. | SUSPICIOUS for unrestricted use, accepted for the bounded tutorial: in-memory storage and static data, plus local directories or genuine AWS S3 over verified HTTPS for the extension. quick-xml 0.38.4 has RUSTSEC-2026-0194 and RUSTSEC-2026-0195: unreachable with in-memory storage, but reachable in the S3 extension, where object_store parses AWS's XML replies. No S3-compatible endpoints, untrusted XML or production deployment. Records: `~/Dev/p/crates-audit/opendata-agent-progress-locked-2026-09-20.md` and `opendata-agent-progress-cloud-2026-09-20.md`. |
| 4 | 2026-10-15 | The Harold daemon's store: turso and the OpenData log, resolved from harold-telemetry at 9da0330 (lock sha256 starts `1dc0119eb5dd7425`; 556 packages, 107 added, 25 removed) | turso =0.8.3 (`default-features = false`, `mimalloc`) with eight sibling crates, opendata-log =1.0.0, opendata-common =0.1.17, OpenTelemetry 0.31 line. | APPROVED WITH CONDITIONS: exact pins, no `sync` feature, no `TESTING=true`, `http-server` off, a Local-only store with absolute paths, rustls >= 0.23.46 and rustls-webpki >= 0.103.15, the resolved lock must match. Exceptions: RUSTSEC-2026-0194 and -0195, and unmaintained bincode, paste and unsafe-libyaml. Nothing was compiled. Record: `.superpowers/audits/opendata-store-2026-10-15/AUDIT.md` in the Harold base checkout (not committed). |
| 5 | 2026-10-16 | Vendored OpenSSL for web-push | openssl-src 300.6.1+3.6.3 (bundles OpenSSL 3.6.3). | CLEAN, risk MEDIUM in general and LOW for Harold's use. OK with conditions: use only through web-push and ece, pin `MACOSX_DEPLOYMENT_TARGET`, check in CI that no libssl or libcrypto is linked dynamically, and track by hand the 24 upstream CVEs that 3.6.3 carries (none filed in RustSec). Record: `~/Dev/p/crates-audit/openssl-src-300.6.1+3.6.3.md`. |
| 6 | 2026-10-20 | `tmx-agent-dash` (summary-strip-readable, a187a59) | chrono =0.4.44, `default-features = false`, feature `clock`. | APPROVE by the rust-supply-chain-auditor. The locked checksum matches the cached crate, there is no build.rs and no new package in `Cargo.lock`. Its RustSec check was from memory, not looked up. No record is written in crates-audit; this row is the record. |

## Status

Early development.
