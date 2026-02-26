# How to set up Harold

## Prerequisites

- macOS (Harold uses iMessage via AppleScript and `ioreg` for screen lock detection)
- tmux — agent sessions must run inside tmux panes
- grpcurl — used by the stop hook to call Harold

  ```bash
  brew install grpcurl
  ```

- An AI CLI — used for summarisation and semantic reply routing. Claude Code is the reference implementation:

  ```bash
  npm install -g @anthropic-ai/claude-code
  ```

- Full Disk Access granted to your terminal app — required to read `~/Library/Messages/chat.db`

## 1. Build and deploy

```bash
make deploy
```

This builds a release binary, code-signs it (required to prevent macOS killing it), and copies the binary, proto file, and default config to `~/bin/harold/`:

```
~/bin/harold/
  harold              # signed binary
  harold.proto        # gRPC service definition (used by grpcurl)
  config/
    default.toml      # shipped defaults
    local.template.toml
```

## 2. Create your local config

```bash
cp ~/bin/harold/config/local.template.toml ~/bin/harold/config/local.toml
```

Edit `local.toml` and fill in your values:

```toml
[imessage]
recipient = "+61400000000"   # your phone number
handle_id = 36               # find with: sqlite3 ~/Library/Messages/chat.db \
                             #   "SELECT ROWID, id FROM handle;"

[ai]
cli_path = "/usr/local/bin/claude"   # path to your AI CLI binary

[tts]
command = "say"
# voice = "Samantha"   # optional — omit to use system default
```

To find your `handle_id`:

```bash
sqlite3 ~/Library/Messages/chat.db "SELECT ROWID, id FROM handle;"
```

Find the row matching your phone number and use its `ROWID`.

## 3. Install the stop hook

Harold is started automatically by the agent stop hook. For Claude Code, add `smart_stop.py` as a Stop hook in `~/.claude/settings.json`:

```json
{
  "hooks": {
    "Stop": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "python3 /path/to/smart_stop.py"
          }
        ]
      }
    ]
  }
}
```

The hook checks whether Harold is running (TCP connect to `127.0.0.1:50060`) and starts it if not, then calls the `TurnComplete` RPC.

## 4. Verify

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

If `screen_locked: true`, Harold will send an iMessage instead of speaking.
