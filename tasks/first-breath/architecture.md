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
│  ┌──────────────────────┐    ┌───────────────────────────────┐   │
│  │  Turn-complete       │    │  Reply routing                │   │
│  │  receiver            │    │                               │   │
│  │                      │    │                               │   │
│  │ - Receives RPC       │    │ - Polls chat.db for replies   │   │
│  │ - Generates summary  │    │ - Semantic resolve via AI CLI │   │
│  │   via AI CLI         │    │ - Falls back to               │   │
│  │ - Detects screen lock│    │   last_notified_pane          │   │
│  │ - Sends iMessage or  │    │ - Final fallback: my-agent    │   │
│  │   triggers TTS       │    │ - Sends keys to tmux pane     │   │
│  │ - Updates            │    │                               │   │
│  │   last_notified_pane │    │                               │   │
│  └──────────────────────┘    └───────────────────────────────┘   │
│                                                                  │
│  Event store (CQRS/event sourcing)                               │
│  State: { last_processed_rowid, last_notified_pane }             │
└──────────────────────────────────────────────────────────────────┘
                    │                        ▲
                    │ iMessage               │ iMessage reply
                    ▼                        │
              Your iPhone/iPad ─────────────┘
```

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

## Responsibilities

| Concern | Owner |
|---|---|
| Transcript parsing | Hook |
| Pane identity (self) | Hook |
| main_context (branch or repo name) | Hook |
| Skip subagent stop events | Hook |
| Ensure harold is running | Hook |
| Screen lock detection | Harold |
| Summarisation (AI CLI) | Harold |
| TTS notification | Harold |
| iMessage send + dedup | Harold |
| `last_notified_pane` state | Harold |
| Reply routing (tmux) | Harold |
| Live pane discovery | Harold |
| Event store | Harold |

---

## Reply routing (inbound)

1. `[tag]` prefix → exact/substring match against live tmux panes
2. No tag, multiple panes → semantic resolve via AI CLI
3. No match → `last_notified_pane`
4. Final fallback → pane whose label contains `my-agent`
5. Nothing found → error iMessage sent back

---

## State

Harold owns all state:

```json
{
  "last_processed_rowid": 12345,
  "last_notified_pane": "%12"
}
```

Routing uses live tmux queries — no stale pane registry.
