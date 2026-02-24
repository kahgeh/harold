# Harold

Harold is a notification and reply routing relay for AI coding agents.

## The problem

Modern AI coding agents are fast enough that running multiple sessions in parallel is practical — one working on a feature, another fixing a bug, another reviewing docs. But keeping track of which session needs your attention, and when, becomes its own problem.

**At your desk:** You're focused on one session while others run in the background. When one completes and needs your input, you miss it. Harold announces completions via voice so you know to switch panes without constantly checking.

**Away from your desk:** Sessions finish with no way to reach you, and you have no way to reply back to the right one. You come back to find everything paused, waiting. Harold notifies you via iMessage with a smart summary of what was done and what's needed, and routes your replies back to the correct session.

Harold solves this by:

- **Announcing completions** via voice (at desk) or iMessage (away) when a turn completes
- **Routing your replies** back to the correct tmux pane, even across multiple running sessions

## How it works

Each agent session runs a stop hook that extracts context from the completed turn and hands it to Harold via gRPC. Harold handles everything from there — summarisation, notification, and routing replies back.

Harold is agent-agnostic. The hook is the only part that knows anything about a specific agent. Harold itself just receives a structured payload and uses the AI CLI for summarisation.

See [tasks/first-breath/architecture.md](tasks/first-breath/architecture.md) for the full architecture.

## Prerequisites

See [docs/prerequisites.md](docs/prerequisites.md).

## Status

Early development.
