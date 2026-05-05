# Prerequisites

## macOS

Harold is currently macOS-only. It relies on macOS-specific features for screen lock detection (`ioreg`) and TTS (`say`). The iMessage channel requires the Messages app and AppleScript; the Telegram channel works cross-platform but Harold itself still requires macOS.

## Required

- **tmux** — sessions must run inside tmux panes
- **grpcurl** — used by agent hooks to call Harold
  ```
  brew install grpcurl
  ```
- **An AI CLI** — used by Harold for summarisation and semantic routing. Claude Code is the reference implementation:
  ```
  npm install -g @anthropic-ai/claude-code
  ```

## Away channel setup

Harold supports two away channels — configure one in `[notify] away_channel`:

### iMessage (default)

- Your Mac must be signed in to iMessage
- Full Disk Access must be granted to the terminal (for reading `~/Library/Messages/chat.db`)
- Set `[imessage] recipient` and `handle_ids` in `local.toml`

### Telegram

- Create a Telegram bot via @BotFather and obtain a bot token
- Set `[telegram] bot_token` and `chat_id` in `local.toml`
- See [Setup Telegram](how-tos/setup-telegram.md) for step-by-step instructions

## Agent hook

Each agent needs a stop hook that calls Harold via `grpcurl` when a turn completes. See the [architecture doc](explanations/architecture.md) for the payload format.

The recommended layout is a shared Harold notifier plus thin per-agent adapters:

- `~/bin/harold/hooks/harold_turn_complete.py` — shared Harold notifier, installed by `make deploy`
- `~/.claude/hooks/turn_complete.py` — Claude Code adapter
- `~/.codex/hooks/turn_complete.py` — Codex adapter
