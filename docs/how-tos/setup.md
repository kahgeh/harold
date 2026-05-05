# How to set up Harold

## Prerequisites

- macOS (Harold uses `ioreg` for screen lock detection; iMessage channel requires AppleScript)
- tmux — agent sessions must run inside tmux panes
- grpcurl — used by the stop hook to call Harold

  ```bash
  brew install grpcurl
  ```

- An AI CLI — used for summarisation and semantic inbound message routing. Claude Code is the reference implementation:

  ```bash
  npm install -g @anthropic-ai/claude-code
  ```

- Full Disk Access granted to your terminal app — required to read `~/Library/Messages/chat.db`

## 1. Set up code-signing

Code-signing is required on macOS because unsigned binaries cannot send iMessages via AppleScript — macOS blocks `osascript` access to Messages.app for untrusted processes.

Run the setup script to select your codesigning identity:

```bash
make setup-codesign
```

This lists available certificates and saves your choice to `.env` (gitignored). If you don't have a certificate, create one in Keychain Access:

1. Open **Keychain Access**
2. **Keychain Access > Certificate Assistant > Create a Certificate**
3. Set Identity Type to **Self Signed Root**
4. Set Certificate Type to **Code Signing**

See [Apple's guide on creating self-signed certificates](https://support.apple.com/en-au/guide/keychain-access/kyca8916/mac) for details.

## 2. Build and deploy

```bash
make deploy
```

This builds a release binary, code-signs it with the identity from step 1, and copies the binary, proto file, and default config to `~/bin/harold/`.

Deployed layout:

```
~/bin/harold/
  harold              # signed binary
  harold.proto        # gRPC service definition (used by grpcurl)
  config/
    default.toml      # shipped defaults
    local.template.toml
```

## 3. Create your local config

```bash
cp ~/bin/harold/config/local.template.toml ~/bin/harold/config/local.toml
```

Edit `local.toml` and fill in your values:

```toml
[imessage]
recipient = "+61400000000"   # your phone number
handle_ids = [36]            # find with: sqlite3 ~/Library/Messages/chat.db \
                             #   "SELECT ROWID, id FROM handle;"

[ai]
cli_path = "/usr/local/bin/claude"   # path to your AI CLI binary

[store]
path = "~/bin/harold/data/events"    # event store location

[tts]
command = "say"
# voice = "Samantha"   # optional — omit to use system default
# fallback_command = "say"   # optional backup when command fails
# fallback_voice = "Samantha"
```

To find your `handle_ids`:

```bash
sqlite3 ~/Library/Messages/chat.db "SELECT ROWID, id FROM handle;"
```

Find the rows matching your phone number and email addresses, and use their `ROWID` values.

## 4. Install agent stop hooks

Harold is notified of completed turns by agent-specific Stop hooks. The hook layout keeps Harold integration shared and leaves transcript parsing to each agent adapter:

```
~/bin/harold/hooks/harold_turn_complete.py   # shared Harold notifier, installed by make deploy
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

It also auto-starts `~/bin/harold/harold` if `HAROLD_ADDR` is not listening, then sends the `TurnComplete` gRPC call via `grpcurl`.

Each adapter should load the shared notifier from Harold's install directory:

```python
import sys
from pathlib import Path

sys.path.insert(0, str(Path.home() / "bin/harold/hooks"))

from harold_turn_complete import TurnComplete, notify_harold
```

### 4a. Register Claude Code

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

### 4b. Register Codex

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

## 5. Verify

Run the diagnostics to confirm everything is wired up:

```bash
~/bin/harold/harold --diagnostics
```

To test the away (iMessage) path, lock your screen first:

```bash
~/bin/harold/harold --diagnostics --delay 10
```

This waits 10 seconds before running — lock your screen during that window.

Expected output:

```
=== Harold diagnostics ===

screen_locked : false
iMessage      : recipient=+61400000000 handle_id=36
TTS           : command=say voice=Some("Samantha")
AI cli        : "/usr/local/bin/claude"

--- Testing notify path (screen_locked=false) ---
Running TTS...
TTS done
```

If `screen_locked: true`, Harold will send an away notification (iMessage or Telegram, per `away_channel` config) instead of speaking.

To use Telegram instead of iMessage, see [Setup Telegram](setup-telegram.md).
