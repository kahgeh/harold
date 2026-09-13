# Prerequisites

Harold currently requires macOS for screen-lock detection and speech. Run installation as your normal user in a graphical login session.

## Release installation tools

The recommended release installer requires an Apple Silicon Mac running macOS 15 or newer, Python 3.9 or newer, tmux, and grpcurl. Agent sessions run inside tmux. The installer also checks macOS tools codesign, launchctl, plutil, and lsof.

With Homebrew available:

```sh
brew install python tmux grpcurl
```

No Rust toolchain or source checkout is needed. Follow [install or reinstall Harold](how-tos/setup.md) for the download command.

## Additional tools for source builds

Only when building from source, add Xcode Command Line Tools, Rust/Cargo, and protoc:

```sh
xcode-select --install
brew install rust protobuf
```

Clone with `--recurse-submodules` to include `events`. The source installer uses the checked-in Cargo lockfile; `--offline` requires those dependencies to be cached already. See [Build from source](how-tos/setup.md#build-from-source).

## Away channel

Choose one channel during installation or supply a local configuration file.

- **iMessage:** sign into Messages, provide the recipient and database handle IDs, and configure macOS database/automation permissions for the installed Harold process.
- **Telegram:** provide a bot token and chat ID. See [Setup Telegram](how-tos/setup-telegram.md).

The installer checks service readiness without sending messages. Channel permissions and delivery must be tested separately.

## Optional AI features

Claude CLI is used for semantic routing and generated summaries when configured. Install and authenticate it before enabling those features. Basic monitoring and notification fallbacks do not require it. Optional activity-summary generation remains disabled in installer-created settings.

## Agent hooks

The installer provides `~/bin/harold/hooks/harold_turn_complete.py`, the shared notifier. Provider-specific transcript adapters and hook registration remain separate; existing provider settings are not overwritten.

See [hook setup](how-tos/setup-agent-monitor-hooks.md) for lifecycle reporting, and [installation setup](how-tos/setup.md#5-connect-agent-hooks) for completion-hook registration.
