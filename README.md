# Harold

Harold is a notification and reply routing relay for AI coding agents.

## The problem

When you're away from your desk, AI coding agents (Claude Code, OpenCode, Codex, etc.) finish turns with no way to reach you — and you have no way to reply back to the right session. You're either staring at your phone waiting, or you come back to find ten sessions paused waiting for input.

Harold solves this by:

- **Notifying you** via iMessage (with a smart summary) when a turn completes
- **Routing your replies** back to the correct tmux pane, even when you have multiple sessions running

## How it works

Each agent session runs a stop hook that extracts context from the completed turn and hands it to Harold via gRPC. Harold handles everything from there — summarisation, notification, and routing replies back.

Harold is agent-agnostic. The hook is the only part that knows anything about a specific agent. Harold itself just receives a structured payload and uses the AI CLI for summarisation.

See [tasks/first-breath/architecture.md](tasks/first-breath/architecture.md) for the full architecture.

## Status

Early development.
