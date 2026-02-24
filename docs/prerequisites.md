# Prerequisites

## macOS

Harold is currently macOS-only. It relies on iMessage (via AppleScript) and the macOS Messages app.

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

## iMessage setup

- Your Mac must be signed in to iMessage
- Full Disk Access must be granted to the terminal (for reading `~/Library/Messages/chat.db`)
- The recipient phone number must be set via the `HAROLD_RECIPIENT` environment variable (e.g. `+61400000000`)

## Agent hook

Each agent needs a stop hook that calls Harold via `grpcurl` when a turn completes. See the [architecture doc](../tasks/first-breath/architecture.md) for the payload format.

A reference hook for Claude Code is provided in `hooks/claude_code/`.
