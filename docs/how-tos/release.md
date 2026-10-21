# Release Harold

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

Every install, by these commands or by `./scripts/install.sh`, moves the previous Harold to `~/bin/harold.backup-<time>` with copies of `haroldctl` and `tmx-agent-dash`, and writes `ROLLBACK.md` there with the commands that put it back. The installer prints that folder. When it prints steps for macOS permissions instead of saying they carry over, follow them; [Keep permissions across installs](setup.md#keep-permissions-across-installs) explains why.
