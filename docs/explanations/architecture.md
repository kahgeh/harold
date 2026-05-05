# Harold Architecture

## Overview

Bidirectional messaging (iMessage or Telegram) ↔ AI coding agent communication, split into two components with clear responsibilities.

Harold's turn-completion notification path is agent-agnostic — it works with any agent that can run a Stop hook and shell out to `grpcurl` to report a completed turn.

---

## Components

```
┌──────────────────────────────────────────────────────────────────┐
│                      AI Agent Session                            │
│                                                                  │
│  Stop hook adapter (Claude, Codex, etc.)                         │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │ - Reads transcript (agent-specific adapter)                 │ │
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
│  ┌─ channels/ ───────────┐  ┌─ outbound/ ─┐  ┌─ inbound/ ──────┐ │
│  │  iMessage + Telegram  │  │ Orchestrator│  │ Inbound msg     │ │
│  │                       │  │             │  │                 │ │
│  │ Each channel owns:    │  │ Tts | Away  │  │ AgentDirectory: │ │
│  │  - send / notify_away │  │             │  │  TmuxProcessScan│ │
│  │  - listen (inbound)   │  │ Screen lock │  │                 │ │
│  │                       │  │ detection   │  │ Semantic resolve│ │
│  │ Static dispatch in    │  │             │  │ via AI CLI      │ │
│  │ channels/mod.rs       │  │ Skip logic  │  │                 │ │
│  │                       │  │ (session/   │  │ Fallback chain: │ │
│  │ Shared utilities:     │  │  pane)      │  │ tag → AI → last │ │
│  │  split_body,          │  │             │  │ notif → my-agent│ │
│  │  summarise_for_notif  │  │             │  │                 │ │
│  └───────────────────────┘  └─────────────┘  └─────────────────┘ │
│                                                                  │
│  Event store (CQRS/event sourcing)                               │
│  State: { last_inbound_rowid, last_self_rowid,                   │
│     last_away_notification_source_agent }                        │
└──────────────────────────────────────────────────────────────────┘
                    │                        ▲
                    │ iMessage / Telegram    │ Reply
                    ▼                        │
                Your phone ──────────────────┘
```

---

## Responsibilities

| Concern                                 | Owner  |
| --------------------------------------- | ------ |
| Transcript parsing                      | Agent adapter hook |
| Pane identity (self)                    | Hook   |
| main_context (branch or repo name)      | Hook   |
| Skip subagent stop events               | Hook   |
| Ensure harold is running                | Hook   |
| Screen lock detection                   | Harold |
| Summarisation (AI CLI)                  | Harold |
| TTS notification                        | Harold |
| iMessage send + dedup                   | Harold |
| `last_notification_source_agent` state  | Harold |
| Inbound message routing (tmux)                    | Harold |
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

1. `skip_if_session_active = true` (default) → skip if the completing pane's tmux session has an attached client
1a. `skip_if_pane_active = false` → skip if the completing pane is the active pane in its session and the screen is unlocked
2. Screen unlocked → TTS via configurable command (e.g. `say`) with an AI-generated short summary
3. Screen locked → away channel (iMessage or Telegram, per `away_channel` config) with a detailed summary via AI CLI; a trailing question is split into a second message

---

## Inbound message routing (inbound)

1. `[tag]` prefix → exact/substring match against discovered live tmux panes
2. No tag, multiple panes → semantic resolve via AI CLI
3. `last_away_notification_source_agent` → the agent whose turn last triggered an away (iMessage) notification
4. Final fallback → pane whose label contains `my-agent`
5. Nothing found → error iMessage sent back

---

## Lifecycle

**Startup** — The agent stop hook checks for a running Harold (TCP connect to the gRPC port) and spawns it if absent, with its working directory set to the binary's parent so config and the event store are found without environment variables.

**Running** — Three concurrent tasks:

1. gRPC server — accepts `TurnComplete` RPCs, appends events
2. Projector — consumes events from the store, drives notification (sets `last_away_notification_source_agent` when away) and inbound message routing
3. Listener — channel-specific inbound message listener: iMessage watches `chat.db` for filesystem changes (FSEvents) with 5s fallback poll; Telegram uses Bot API long-polling. Both append `InboundMessageReceived` events

**Shutdown** — SIGINT or SIGTERM triggers an ordered shutdown:

1. gRPC server stops accepting new requests
2. Projector and listener tasks drain and exit
3. WAL checkpoint flushes all pending writes to the main database files

The checkpoint ensures the next startup opens a clean database without replaying WAL pages.

---

## State

Harold owns all routing state in-memory:

- `last_inbound_rowid` / `last_self_rowid` — separate chat.db polling cursors for inbound messages and self-sent (phone-synced) messages
- `last_away_notification_source_agent: Option<AgentAddress>` — the agent whose turn completion last triggered an away (iMessage) notification

`AgentAddress` is an enum (currently only `TmuxPane { pane_id, label }`), extensible to other transports.

Live pane discovery uses live tmux queries.
