# Install or Reinstall Harold

Install Harold and its dashboard from the latest macOS release. This is the recommended path: it downloads ready-built programs, checks their checksum, and asks for your channel settings. Rust and a source checkout are unnecessary.

The installer starts Harold once to verify installation. Later, start it when needed with `haroldctl start`; nothing is registered for automatic login startup.

## Prerequisites

- An Apple Silicon Mac running macOS 15 or newer, with an active graphical login session. Run the installer as your normal user.
- Python 3.9 or newer, tmux, and grpcurl.
- A configured iMessage account or Telegram bot for the away channel.

With Homebrew available, install the runtime tools:

```sh
brew install python tmux grpcurl
```

The installer checks prerequisites and reports missing tools; it does not install packages for you. It also uses macOS tools such as codesign and launchctl. See [prerequisites](../prerequisites.md) for channel requirements and optional AI features.

## 1. Install

<a id="install-a-published-release-without-rust"></a>

```sh
curl -fsSL https://raw.githubusercontent.com/kahgeh/harold/main/scripts/bootstrap.sh | sh
```

This downloads the latest stable release and verifies its SHA-256 checksum before installation. To compile local changes instead, use [Build from source](#build-from-source).

## 2. Configure

On the first run, choose iMessage or Telegram and enter the requested channel settings. Telegram tokens are entered without echo. Alternatively, supply a complete local configuration file:

```sh
curl -fsSL https://raw.githubusercontent.com/kahgeh/harold/main/scripts/bootstrap.sh | sh -s -- --config /path/to/local.toml
```

Without a terminal, supply `--config PATH` for the first installation. Existing configuration is retained by default.

The installer validates configuration before replacing the installed program. It uses ad-hoc code signing by default; to use your own signing identity:

```sh
curl -fsSL https://raw.githubusercontent.com/kahgeh/harold/main/scripts/bootstrap.sh | sh -s -- --signing-identity 'My Code Signing Certificate'
```

Signing does not grant macOS privacy permissions. For iMessage, configure the installed Harold process's access to the Messages database and approve Messages automation when macOS requests it. A service managed by launchd does not run inside your terminal.

The default layout is:

```text
~/bin/harold/
  harold
  harold.proto
  service.py
  service.json
  service.plist
  config/default.toml
  config/local.toml
  config/local.template.toml
  hooks/harold_turn_complete.py
  data/events/
  harold.log
~/bin/haroldctl
~/bin/tmx-agent-dash
```

`local.toml` holds your channel and optional feature settings. The installed service uses its own `config` directory and `data/events` store, regardless of paths copied from another machine or Harold overrides in the invoking shell. It records the PATH and UTF-8 locale needed by its tools.

Add `~/bin` to your shell's PATH if needed, or use the absolute commands below. The installer prints the service definition and installation paths. The service definition stays inside the installation rather than the auto-loaded LaunchAgents directory. A custom location can be selected with `--prefix /absolute/path/to/bin`.

## 3. Verify and control the service

```sh
~/bin/haroldctl status
```

Success requires both the managed process owning the listening socket and Harold returning a current agent-state snapshot. The installer performs this same check before reporting success. It does not send a test notification.

Start the dashboard from inside tmux:

```sh
~/bin/tmx-agent-dash
```

If you configure a different gRPC endpoint, pass it explicitly, for example `--endpoint http://127.0.0.1:50061`.

Service commands are:

```sh
~/bin/haroldctl stop
~/bin/haroldctl start
~/bin/haroldctl restart
```

After changing `~/bin/harold/config/local.toml`, restart Harold. For a startup failure, inspect `~/bin/harold/harold.log`; configuration errors and an incompatible existing database require correction before readiness can succeed.

## 4. Reinstall or update

To replace the programs while retaining your settings and managed database, run the installer again:

```sh
curl -fsSL https://raw.githubusercontent.com/kahgeh/harold/main/scripts/bootstrap.sh | sh
```

To retain settings but start with a fresh database:

```sh
curl -fsSL https://raw.githubusercontent.com/kahgeh/harold/main/scripts/bootstrap.sh | sh -s -- --reinstall
```

Reinstall archives the previous installation, including its data, and prints the backup location. It creates a fresh managed store; it does not convert old schemas or delete an external store named in a copied configuration. Use `--config` as well if you want to replace the local settings.

## 5. Connect agent hooks

The core installation includes the shared notifier. Copy the provider-specific transcript adapters from your existing setup to the paths below before registering them. Those adapters and their registration are separate: the installer does not overwrite your Claude, Codex, or OpenCode settings. The [agent-monitor hook guide](setup-agent-monitor-hooks.md) covers lifecycle reporting and the opt-in OpenCode plugin.

Harold is notified of completed turns by agent-specific Stop hooks. The hook layout keeps Harold integration shared and leaves transcript parsing to each agent adapter:

```
~/bin/harold/hooks/harold_turn_complete.py   # shared Harold notifier, installed by the installer
~/.claude/hooks/turn_complete.py             # Claude transcript adapter
~/.codex/hooks/turn_complete.py              # Codex transcript adapter
```

The adapter parses its agent's hook payload and transcript, then calls the shared notifier with:

| Field               | Source                                                        |
| ------------------- | ------------------------------------------------------------- |
| `cwd`               | Agent hook payload or current working directory               |
| `last_user_prompt`  | Last user message from that agent's transcript format         |
| `assistant_message` | Current turn's final assistant/agent message                  |

The shared notifier adds the Harold-specific fields:

| Field          | Source                                                       |
| -------------- | ------------------------------------------------------------ |
| `pane_id`      | `TMUX_PANE` environment variable                             |
| `pane_label`   | `tmux display-message` (e.g. `harold:0.3`)                   |
| `main_context` | Git branch name, or repo name when on `main`                 |

The shared notifier asks `~/bin/haroldctl start` to ensure the managed service is ready, then sends `TurnComplete` through `grpcurl` to the installed endpoint. It does not launch another daemon.

Each adapter should load the shared notifier from Harold's install directory:

```python
import sys
from pathlib import Path

sys.path.insert(0, str(Path.home() / "bin/harold/hooks"))

from harold_turn_complete import TurnComplete, notify_harold
```

### Register Claude Code

Add the Stop hook to `~/.claude/settings.json`. Use the absolute path for your home directory:

```json
{
  "hooks": {
    "Stop": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "uv run /Users/<you>/.claude/hooks/turn_complete.py"
          }
        ]
      }
    ]
  }
}
```

The Claude adapter skips `SubagentStop`, prefers `last_assistant_message` from the hook payload, and falls back to parsing the Claude JSONL transcript.

### Register Codex

Enable Codex hooks and register the Stop hook in `~/.codex/config.toml`. Use the absolute path for your home directory:

```toml
[features]
codex_hooks = true

[[hooks.Stop]]

[[hooks.Stop.hooks]]
type = "command"
command = "uv run /Users/<you>/.codex/hooks/turn_complete.py"
timeout = 15
statusMessage = "Notifying Harold"
```

The Codex adapter parses Codex transcript events and supports both `event_msg` user/agent messages and `response_item` role-based messages.

## 6. Test notifications when ready

The service readiness check establishes that Harold is running. It does not prove that macOS permissions, provider hooks, or channel credentials work end to end. After completing those steps, submit a controlled task through a configured agent and verify the dashboard and expected notification.

The daemon also provides an explicit diagnostic command that can speak or send an away-channel message. Run it only when you intend to test those effects:

```sh
~/bin/harold/harold --diagnostics
```

For Telegram configuration details, see [Setup Telegram](setup-telegram.md).

## Build from source

Use this path to install local code changes. In addition to the runtime tools above, install Xcode Command Line Tools, Rust/Cargo, and protoc:

```sh
xcode-select --install
brew install rust protobuf
```

Get the source and its dependency:

```sh
git clone --recurse-submodules https://github.com/kahgeh/harold.git
cd harold
./scripts/install.sh
```

If an existing checkout is missing `events`, run `git submodule update --init --recursive` first. The source installer builds both programs using the checked-in Cargo lockfile, then performs the same configuration and readiness checks as the release installer.

Run `./scripts/install.sh` again to install the current checkout while retaining settings and data. Use `./scripts/install.sh --reinstall` for fresh storage with retained settings.

`make install` and `make deploy` use the same normal-install path. `make reinstall` selects fresh storage. Additional flags can be passed as `INSTALL_ARGS`, for example:

```sh
make reinstall INSTALL_ARGS='--offline'
```

`--offline` requires the locked Cargo dependencies to be cached already.
