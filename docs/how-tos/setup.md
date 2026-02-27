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

[store]
path = "~/bin/harold/data/events"    # event store location

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

Harold is notified of completed agent turns via a Claude Code [Stop hook](https://code.claude.com/docs/en/hooks). The hook also auto-starts Harold if it is not already running.

### 3a. Create the hook script

Save the following as `~/.claude/hooks/smart_stop.py`:

```python
#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///

"""
Stop hook — extracts turn context and notifies Harold via gRPC.
All notification logic (TTS, iMessage, routing) lives in Harold.
"""

import json
import os
import re
import subprocess
import sys
from pathlib import Path

HAROLD_ADDR = os.getenv("HAROLD_ADDR", "localhost:50060")
HAROLD_PROTO = str(Path.home() / "bin/harold/harold.proto")
HAROLD_BINARY = str(Path.home() / "bin/harold/harold")


def extract_last_user_prompt(transcript_path: str) -> str:
    """Extract the last user prompt from the JSONL transcript."""
    if not transcript_path or not os.path.exists(transcript_path):
        return ""
    prompts = []
    with open(transcript_path) as f:
        for line in f:
            try:
                data = json.loads(line)
                if data.get("type") != "user":
                    continue
                content = data.get("message", {}).get("content")
                if isinstance(content, str) and not content.startswith("<"):
                    text = content.strip()
                    if text and "tool_use_id" not in text:
                        prompts.append(text)
            except (json.JSONDecodeError, KeyError):
                continue
    return prompts[-1][:500] if prompts else ""


def get_pane_info() -> tuple[str, str]:
    """Get the tmux pane ID and label from the environment."""
    pane_id = os.environ.get("TMUX_PANE", "")
    if not pane_id:
        return "", "unknown"
    try:
        result = subprocess.run(
            ["tmux", "display-message", "-t", pane_id, "-p",
             "#{session_name}:#{window_index}.#{pane_index}"],
            capture_output=True, text=True, timeout=3,
        )
        if result.returncode == 0 and result.stdout.strip():
            label = re.sub(r"[^\x20-\x7e]", "", result.stdout.strip())
            label = re.sub(r"\s+", " ", label).strip()
            return pane_id, label
    except Exception:
        pass
    return pane_id, "unknown"


def get_main_context() -> str:
    """Derive context from git branch or repo name."""
    try:
        branch = subprocess.run(
            ["git", "rev-parse", "--abbrev-ref", "HEAD"],
            capture_output=True, text=True, timeout=2,
        ).stdout.strip()
        if branch and branch != "main":
            return branch
        url = subprocess.run(
            ["git", "config", "--get", "remote.origin.url"],
            capture_output=True, text=True, timeout=2,
        ).stdout.strip()
        if url:
            name = url.rstrip("/").rstrip(".git").rsplit("/", 1)[-1]
            if name:
                return name
    except Exception:
        pass
    return os.path.basename(os.getcwd())


def ensure_harold_running():
    """Start Harold if it is not already listening."""
    try:
        import socket
        host, port = HAROLD_ADDR.rsplit(":", 1)
        with socket.create_connection((host, int(port)), timeout=1):
            return
    except Exception:
        pass
    subprocess.Popen(
        [HAROLD_BINARY],
        cwd=str(Path(HAROLD_BINARY).parent),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    import time
    time.sleep(1)


def call_harold(pane_id, pane_label, last_user_prompt,
                assistant_message, main_context):
    """Send TurnComplete RPC to Harold via grpcurl."""
    payload = json.dumps({
        "pane_id": pane_id,
        "pane_label": pane_label,
        "last_user_prompt": last_user_prompt,
        "assistant_message": assistant_message,
        "main_context": main_context,
    })
    subprocess.run(
        [
            "grpcurl", "-plaintext",
            "-import-path", str(Path(HAROLD_PROTO).parent),
            "-proto", Path(HAROLD_PROTO).name,
            "-d", payload,
            HAROLD_ADDR, "harold.Harold/TurnComplete",
        ],
        capture_output=True, timeout=10,
    )


def main():
    try:
        input_data = json.load(sys.stdin)

        if input_data.get("hook_event_name") == "SubagentStop":
            sys.exit(0)

        transcript_path = input_data.get("transcript_path", "")
        pane_id, pane_label = get_pane_info()
        last_user_prompt = extract_last_user_prompt(transcript_path)

        # Prefer last_assistant_message from hook input — guaranteed
        # to be the current turn's response. Fall back to transcript
        # parsing for older Claude Code versions that lack this field.
        assistant_message = input_data.get("last_assistant_message", "")
        if not assistant_message:
            assistant_message = extract_last_assistant_message(transcript_path)

        main_context = get_main_context()

        ensure_harold_running()
        call_harold(pane_id, pane_label, last_user_prompt,
                    assistant_message, main_context)
        sys.exit(0)

    except (json.JSONDecodeError, Exception):
        sys.exit(0)


if __name__ == "__main__":
    main()
```

The hook extracts five pieces of context from each completed turn:

| Field               | Source                                                            |
| ------------------- | ----------------------------------------------------------------- |
| `pane_id`           | `TMUX_PANE` environment variable                                  |
| `pane_label`        | `tmux display-message` (e.g. `harold:0.3`)                       |
| `last_user_prompt`  | Last user message from the JSONL transcript                       |
| `assistant_message` | `last_assistant_message` from Stop hook input (current turn)      |
| `main_context`      | Git branch name, or repo name when on `main`                     |

### 3b. Register the hook

Add the Stop hook to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "Stop": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "uv run ~/.claude/hooks/smart_stop.py"
          }
        ]
      }
    ]
  }
}
```

The hook runs on every Stop event (empty matcher). It auto-starts Harold via TCP probe to `localhost:50060`, then sends the `TurnComplete` gRPC call.

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
