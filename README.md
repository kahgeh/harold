# Harold

Stop letting your agents nap. Harold notifies you of idling agents in another room while you are at your desk or even when you are away. When you are away, Harold can direct your replies to your agents, ensuring the work never stops just because you stepped out for a coffee.

The "Stalled Momentum" Problem
At your desk: You’re deep in the zone on Pane 1. Meanwhile, the tasks in Panes 2 and 3 finished five minutes ago and are now just drawing digital breath. Harold breaks the silence by announcing completions via voice, letting you jump in, give the next command, and keep the gears turning without having to manually poll your tabs like a nervous intern.

Away from your desk: A long-running doc-gen or complex fix finishes while you're out. Normally, that agent stays dormant until you get back. Harold pings your iMessage with a concise summary. You reply from your phone, Harold surgically routes the text back to the specific tmux session, and the agent is back to work before you’ve even finished your latte.

## Harold’s Prime Directives

Zero Idle Time: Voice announcements of turn completion.

Remote Command: Use iMessage to feed the beast from anywhere. Review a summary, text a correction, and keep the session moving.

Contextual Routing: Don't worry about which agent is which. Harold knows exactly where your input belongs, ensuring your workflow stays unbroken across every tmux pane.

## How it works

Harold inventories configured agent processes in tmux, combines explicit lifecycle hooks with provider-specific visible-screen evidence, and stores the resulting current state durably. Existing stop hooks also report completed turns for notification and non-destructive work-summary updates.

The public agent-state stream starts with a complete current snapshot, then sends later revisions. Raw pane captures remain inside the screen adapter; only classified state and a normalized work summary can cross the acquisition boundary.

See the [architecture explanation](docs/explanations/architecture.md), [agent-monitor reference](docs/references/agent-monitor/README.md), and [hook setup guide](docs/how-tos/setup-agent-monitor-hooks.md).

## Dashboard

The [`tmx-agent-dash`](tmx-agent-dash/README.md) terminal dashboard shows Harold's current agent-pane projection and can switch the invoking tmux client to a selected pane.

Install the latest release on an Apple Silicon Mac (macOS 15+):

```sh
curl -fsSL https://raw.githubusercontent.com/kahgeh/harold/main/scripts/bootstrap.sh | sh
```

Requires Python 3.9+, tmux and grpcurl (`brew install python tmux grpcurl`).
The bootstrap verifies the release checksum and opens the configuration wizard.
For a source build, run `./scripts/install.sh` from a checkout.

The installer verifies readiness and provides `haroldctl start` for on-demand startup. It does not register automatic login startup. Use `./scripts/install.sh --reinstall` for fresh storage with retained settings. See [installation and reinstallation](docs/how-tos/setup.md) for another-Mac setup.

Then start the dashboard inside the tmux client it should navigate:

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

## Status

Early development.
