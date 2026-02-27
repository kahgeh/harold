# Harold Architecture

## Overview

Bidirectional iMessage ↔ AI coding agent communication, split into two components with clear responsibilities.

Harold is agent-agnostic — it works with any agent that can shell out to `grpcurl` to report a completed turn.

---

## Components

```
┌──────────────────────────────────────────────────────────────────┐
│                      AI Agent Session                            │
│                                                                  │
│  Stop hook (e.g. smart_stop.py for Claude Code)                  │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │ - Reads transcript (agent-specific knowledge)               │ │
│  │ - Extracts last user prompt + agent final message           │ │
│  │ - Gets pane_id + label from tmux                            │ │
│  │ - Computes main_context from git (branch or repo name)      │ │
│  │ - Skips subagent stop events                                │ │
│  │ - Ensures harold is running (starts if not)                 │ │
│  │ - Calls harold via grpcurl (TurnComplete RPC)               │ │
│  └─────────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────────┘
                              │
                              │ gRPC (grpcurl)
                              │ TurnComplete RPC
                              ▼
┌──────────────────────────────────────────────────────────────────┐
│                          Harold                                  │
│                       (Rust binary)                              │
│                                                                  │
│  ┌─ outbound/ ──────────┐    ┌─ inbound/ ────────────────────┐   │
│  │  Notification        │    │  Reply routing                │   │
│  │                      │    │                               │   │
│  │ OutboundChannel:     │    │ AgentDirectory:               │   │
│  │   Tts | IMessage     │    │   TmuxProcessScan             │   │
│  │                      │    │   → discover(), is_alive()    │   │
│  │ - Generates summary  │    │                               │   │
│  │   via local model    │    │ AgentAddress (= the channel): │   │
│  │ - Detects screen lock│    │   TmuxPane { pane_id, label } │   │
│  │ - Sends iMessage or  │    │   → relay(), label()          │   │
│  │   triggers TTS       │    │                               │   │
│  │ - Returns source     │    │ - Polls chat.db for replies   │   │
│  │   agent for routing  │    │ - Semantic resolve via AI CLI │   │
│  │   state update       │    │ - Falls back to               │   │
│  │                      │    │   last_routed_agent, then     │   │
│  │                      │    │   last_away_notification_     │   │
│  │                      │    │   source_agent, then my-agent │   │
│  └──────────────────────┘    └───────────────────────────────┘   │
│                                                                  │
│  Event store (CQRS/event sourcing)                               │
│  State: { last_inbound_rowid, last_self_rowid,                   │
│     last_routed_agent, last_away_notification_source_agent }     │
└──────────────────────────────────────────────────────────────────┘
                    │                        ▲
                    │ iMessage               │ iMessage reply
                    ▼                        │
              Your iPhone/iPad ──────────────┘
```

---

## Responsibilities

| Concern                                 | Owner  |
| --------------------------------------- | ------ |
| Transcript parsing                      | Hook   |
| Pane identity (self)                    | Hook   |
| main_context (branch or repo name)      | Hook   |
| Skip subagent stop events               | Hook   |
| Ensure harold is running                | Hook   |
| Screen lock detection                   | Harold |
| Summarisation (AI CLI)                  | Harold |
| TTS notification                        | Harold |
| iMessage send + dedup                   | Harold |
| `last_notification_source_agent` state  | Harold |
| `last_routed_agent` state               | Harold |
| Reply routing (tmux)                    | Harold |
| Live pane discovery                     | Harold |
| Event store                             | Harold |

---

## TurnComplete RPC payload

```protobuf
message TurnCompleteRequest {
  string pane_id            = 1;  // tmux pane ID (e.g. "%12")
  string pane_label         = 2;  // human-readable label (e.g. "alir-app main:0.1")
  string last_user_prompt   = 3;  // last thing the user asked
  string assistant_message  = 4;  // agent's final response
  string main_context       = 5;  // git branch or repo name
}
```

---

## Notification (outbound)

When a `TurnCompleted` event is received, Harold decides how to notify:

1. `skip_if_session_active = true` (default) → skip if the user is in an active tmux session
2. Screen unlocked → TTS via configurable command (e.g. `say`) with an AI-generated short summary
3. Screen locked → iMessage with a detailed summary via AI CLI; a trailing question is split into a second message

---

## Reply routing (inbound)

1. `[tag]` prefix → exact/substring match against live tmux panes
2. No tag, multiple panes → semantic resolve via AI CLI
3. `last_routed_agent` → the agent last successfully delivered a reply to
4. `last_away_notification_source_agent` → the agent whose turn last triggered an away (iMessage) notification
5. Final fallback → pane whose label contains `my-agent`
6. Nothing found → error iMessage sent back

---

## Lifecycle

**Startup** — The agent stop hook checks for a running Harold (TCP connect to the gRPC port) and spawns it if absent, with its working directory set to the binary's parent so config and the event store are found without environment variables.

**Running** — Three concurrent tasks:

1. gRPC server — accepts `TurnComplete` RPCs, appends events
2. Projector — consumes events from the store, drives notification (sets `last_away_notification_source_agent` when away) and reply routing (sets `last_routed_agent`)
3. Listener — polls `chat.db` every 5 s for new inbound and self-sent iMessages using separate cursors, appends `ReplyReceived` events

**Shutdown** — SIGINT or SIGTERM triggers an ordered shutdown:

1. gRPC server stops accepting new requests
2. Projector and listener tasks drain and exit
3. WAL checkpoint flushes all pending writes to the main database files

The checkpoint ensures the next startup opens a clean database without replaying WAL pages.

---

## State

Harold owns all routing state in-memory:

- `last_inbound_rowid` / `last_self_rowid` — separate chat.db polling cursors for inbound messages and self-sent (phone-synced) messages
- `last_routed_agent: Option<AgentAddress>` — the agent a reply was last successfully delivered to
- `last_away_notification_source_agent: Option<AgentAddress>` — the agent whose turn completion last triggered an away (iMessage) notification

`AgentAddress` is an enum (currently only `TmuxPane { pane_id, label }`), extensible to other transports.

Live pane discovery uses live tmux queries.
