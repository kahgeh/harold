# Prerequisites

Harold currently requires macOS for screen-lock detection and speech. Run installation as your normal user in a graphical login session.

## Build and runtime tools

The source installer needs Xcode Command Line Tools, Rust/Cargo, Python 3.9 or newer, tmux, grpcurl, and protoc. It also checks the macOS tools codesign, launchctl, plutil, and lsof. Agent sessions run inside tmux.

With Homebrew available:

```sh
xcode-select --install
brew install rust python tmux grpcurl protobuf
```

Clone the repository with `--recurse-submodules` to include `events`. The installer builds using the checked-in Cargo lockfile; `--offline` requires those dependencies to be cached already. See [install or reinstall Harold](how-tos/setup.md).

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
