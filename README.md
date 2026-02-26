# Harold

Stop letting your agents nap. Harold notifies you of idling agents in another room while you are at your desk or even when you are away. When you are away, Harold can direct your replies to your agents, ensuring the work never stops just because you stepped out for a coffee.

The "Stalled Momentum" Problem
At your desk: You’re deep in the zone on Pane 1. Meanwhile, the tasks in Panes 2 and 3 finished five minutes ago and are now just drawing digital breath. Harold breaks the silence by announcing completions via voice, letting you jump in, give the next command, and keep the gears turning without having to manually poll your tabs like a nervous intern.

Away from your desk: A long-running doc-gen or complex fix finishes while you're out. Normally, that agent stays dormant until you get back. Harold pings your iMessage with a concise summary. You reply from your phone, Harold surgically routes the text back to the specific tmux session, and the agent is back to work before you’ve even finished your latte.

## Harold’s Prime Directives

Zero Idle Time: Voice announcements of turn completion.

Remote Command: Use iMessage to feed the beast from anywhere. Review a summary, text a correction, and keep the session moving.

Contextual Routing: Don't worry about which agent is which. Harold knows exactly where your input belongs, ensuring your workflow stays unbroken across every tmux pane.

## How it works

Each agent session runs a stop hook that extracts context from the completed turn and hands it to Harold via gRPC. Harold handles everything from there — summarisation, notification, and routing replies back.

Harold is agent-agnostic. The hook is the only part that knows anything about a specific agent. Harold itself just receives a structured payload and uses the AI CLI for summarisation.

See [docs/explanations/architecture.md](docs/explanations/architecture.md) for the full architecture.

## Prerequisites

See [docs/prerequisites.md](docs/prerequisites.md).

## Status

Early development.
