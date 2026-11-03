# Prerequisites

Harold requires macOS for screen-lock detection and speech. Linux has neither yet: see [Linux prerequisites](#linux-prerequisites). Run installation as your normal user in a graphical login session.

## Build features

Harold is built from Cargo features, one for each platform part. A build chooses its features. A setting does not.

| Feature | What it builds in | In `mac-defaults` | In `linux-defaults` |
| --- | --- | --- | --- |
| `imessage` | The iMessage channel. macOS only. | yes | no |
| `calls` | Calls at the desk and in the web app, and the echo test page. macOS only. | yes | no |

- `mac-defaults` is the default set, and the Mac package uses it.
- `linux-defaults` is the set for the Linux package. It has no feature yet, because both features need macOS.
- Build another mix by hand. For example, `cargo build --no-default-features --features calls` builds a Mac without iMessage.
- Run `harold --build-features` to print the features of a build, one on each line.
- A build refuses a setting that needs a missing feature, and names the setting. Remove the setting from `config/local.toml`.
- Before every merge and release, run `scripts/check_features.sh`. It checks `linux-defaults`, no feature and each feature alone.

## Release installation tools

The recommended release installer requires an Apple Silicon Mac running macOS 15 or newer, Python 3.9 or newer, tmux, and grpcurl. Agents run inside tmux. The installer also checks macOS tools codesign, launchctl, plutil, and lsof.

With Homebrew available:

```sh
brew install python tmux grpcurl
```

No Rust toolchain or source checkout is needed. Follow [install or reinstall Harold](how-tos/setup.md) for the download command.

## Linux prerequisites

The Linux install needs Arch Linux or omarchy on x86_64, Python 3.9 or newer, and a running Rex server. Agents run in Rex. Run the installer as your normal user, in a login session.

The installer checks for `rex`, `grpcurl`, `lsof`, `systemctl` and `curl`. `systemctl` comes with Arch. Install the others with your own `sudo pacman` and AUR steps:

```sh
sudo pacman -S --needed python curl lsof inetutils base-devel pipewire
```

Install `grpcurl` from the AUR package `grpcurl-bin`, with `makepkg -si`, after you approve its check. A machine that already has the AUR package `grpcurl` also works. That package conflicts with `grpcurl-bin`, so never install both (row 16 of the [dependency audits](references/dependency-audits.md)). The installer does not run `sudo`, `pacman` or `makepkg`. See [install on Linux](how-tos/setup.md#install-on-linux).

The Linux install has no speech, no web app and no line to the phone. Later stages add them. The installer does not check for `codesign`, `launchctl` or `plutil`.

## Additional tools for source builds

Only when building from source, add Xcode Command Line Tools, Rust/Cargo, and protoc:

```sh
xcode-select --install
brew install rust protobuf
```

The source installer uses the checked-in Cargo lockfile; `--offline` requires those dependencies to be cached already. See [Build from source](how-tos/setup.md#build-from-source).

## Away channel

The away channel is iMessage. During installation, or in a local configuration file: sign into Messages, provide the recipient and database handle IDs, and configure macOS database/automation permissions for the installed Harold process.

The installer checks service readiness without sending messages. Channel permissions and delivery must be tested separately.

## Optional AI features

Claude CLI is used for semantic routing and activity summaries when configured. Install and authenticate it before enabling those features. Basic monitoring and turn-end update fallbacks do not require it. Optional activity-summary generation remains disabled in installer-created settings.

## Agent hooks

Starting with v0.1.1, the installer provides `claude_turn_complete.py`, `codex_turn_complete.py`, and the shared `harold_turn_complete.py` in `~/bin/harold/hooks/`. All use Python 3.9+ without uv. Hook registration remains a separate step; existing provider settings are not overwritten.

See [hook setup](how-tos/setup-agent-monitor-hooks.md) for lifecycle reporting, and [installation setup](how-tos/setup.md#5-connect-agent-hooks) for completion-hook registration.
