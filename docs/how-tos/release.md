# Release Harold

## Download release builds

Publishing a GitHub release triggers the **Build** workflow. After
its build and checks succeed, it attaches `harold-aarch64-apple-darwin.tar.gz`
and its `.sha256` checksum to that release. The archive contains `harold`,
`control-tower`, configuration templates, the gRPC schema, the source
revision and the release version. Draft releases, pushes, and pull requests do not trigger builds.
Re-running a release workflow replaces its matching assets.

The workflow has two jobs. The `build` job builds and uploads the macOS package. The `linux` job builds, packages and smoke-tests Harold on Linux, and uploads the Linux package. If the `linux` job fails, the run fails, and `make release` does not mark the release latest. The release run only builds, checks the binaries and uploads them: the tests run on each push to `main`, and the release run keeps no cache.

The macOS `Build release binaries` step has a `timeout-minutes` limit of 20, so a hung build step fails in minutes. Each job has a 30-minute job limit as the outer bound.

The **push checks** are the **Linux tests** workflow (`linux-tests.yml`) and the **macOS checks** workflow (`macos-checks.yml`). Both run on each push to `main`, with a cache of the compiled dependencies. Each job restores the cache before the build and saves it after, and the save runs even when the build step fails or times out. The `Test` step has a limit of 40 minutes. The `Check the feature mixes` step has a limit of 30 minutes. `make release` makes a release only when both workflows have a push run with the conclusion `success` for the exact commit it releases ([ADR 0033](../adr/0033-a-release-trusts-the-checks-of-each-push.md), which supersedes the gate of [ADR 0026](../adr/0026-a-release-publishes-only-a-commit-whose-linux-tests-passed.md)). So a failure on either system stops a release before anything is public.

These builds target Apple Silicon Macs running macOS 15 or newer. Binaries are
ad-hoc signed, not notarized. From v0.1.1, the archive includes the installer, service controller, Claude and Codex completion scripts, and shared notifier. The
bootstrap ([install or reinstall Harold](setup.md)) installs these together without Rust or a source checkout. The release also
attaches a Linux package, `harold-x86_64-unknown-linux-gnu.tar.gz`, which the Mac installer does not use. A published release containing the installer is required.

### Cut a release

Run `make release` in this checkout to publish the pushed `main` as the next release and install it on this Mac. It first checks that the checkout is on `main`, that no tracked file has uncommitted changes, that `main` matches `origin/main`, that `gh` is signed in, that the new tag is free, and that both push checks passed on the commit. It then runs only the timing tests on this Mac. If a push check is still running, failed or missing, it prints the run, or the missing workflow, to wait for. When a check fails it prints why and publishes nothing.

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

Every install, by these commands or by `./scripts/install.sh`, moves the previous Harold to `~/bin/harold.backup-<time>` with copies of `haroldctl` and, when it exists, `tmx-agent-dash`, and writes `ROLLBACK.md` there with the commands that put it back. The installer prints that folder. When the installer prints steps for macOS permissions, follow them. When the permissions carry over, no steps appear. [Keep permissions across installs](setup.md#keep-permissions-across-installs) explains why.

A rollback to an install from before that move returns the old program. The backup holds `tmx-agent-dash` when the install replaced a layout from before that move. `ROLLBACK.md` names it. Its commands put it back in `~/bin`.

### Roll back across the agent state rename

The agent state rename changes the events Harold writes. An older Harold cannot read the events that this Harold writes.

To roll back, put back the old bundle and its copied data folder as `ROLLBACK.md` tells. The installer copies `data/store` and `data/events` into the backup folder. The old Harold reads that copy. The new data stays in the folder you move aside.

Replies and events from after the install are lost on a rollback. If the copy fails, the install puts the old bundle back and starts the old Harold.

The old name `AGENT_STATE_BUSY` stays readable through the proto alias. Refresh the opencode plugin after this install.

### Restart the control-tower after the stage 3 install

The install of stage 3 prints this line after `Harold ready at ...`:

```
restart the control-tower to see blocked, done and error
```

A control-tower that started before the install shows `UNKNOWN` for these three states until you restart it.

### Roll back from stage 3 to stage 2 or 1

A rollback from stage 3 needs no data copy, because stage 1 already changed the events and the store. A Harold of stage 2 or 1 reads the event words `Blocked`, `Done` and `Error` as unknown, and the store accepts all six states. Put back the old bundle as `ROLLBACK.md` tells. Start the control-tower again after the rollback.
