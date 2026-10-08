# Inbound Message Routing

Inbound message routing routes messages from your phone (via iMessage or Telegram, depending on the configured away channel) to the correct agent session running in a tmux pane.

## Problem

Messaging from your phone means you know which agent you meant but the message arrives as plain text with no session context. With multiple agent sessions running, there is no obvious way to get your message to the right one.

## Architecture

Routing has two stages: inbound collection and routing resolution.

**Inbound collection** — The listener is channel-specific, selected by `[notify] away_channel`:

**iMessage** — Watches `chat.db` for filesystem changes (via FSEvents on macOS) and runs two separate queries on each change, each with its own ROWID cursor. A 5-second fallback poll ensures messages are still detected if the filesystem watcher is unavailable:

- **Inbound** — `handle_id IN (handle_ids) AND is_from_me = 0` — messages sent by the user from the recipient's device
- **Self** — `handle_id IN (handle_ids) AND is_from_me = 1` — the sent copy of a message the Mac sends to the user's own number, which includes replies typed in Messages on the Mac

Each cursor is advanced only after a successful `append_inbound_message`, so a crash before the append causes the message to be reprocessed on the next poll rather than skipped.

A reply typed in Messages on the Mac is stored as two rows with the same text, one matched by each query, so the listener records a row only once when both arrive: a row is skipped if a row with identical text from the other query was recorded within the last 10 seconds and has not already been matched this way. A skipped row still advances its cursor but appends no event. Two rows with the same text from the same query are both recorded (the user sent it twice), and rows discarded while messaging is paused are not remembered.

**Telegram** — Long-polls the Telegram Bot API `getUpdates` endpoint (30s timeout). On startup, drains any pre-existing updates to avoid replaying old messages. Only messages from the configured `chat_id` are processed; messages starting with `🤖` (Harold's own messages) are filtered out.

**Routing resolution** — Harold's event handler stages `InboundMessageReceived` events in its durable outbox and calls `route_inbound_message()` in stream-version order. Live pane discovery runs at resolution time via `tmux list-panes -a`, then reads the process tree under each pane's `pane_pid`. A pane is considered an agent when the pane process or a descendant process command contains one of the configured `agents[].command_contains` fragments. Agents are addressed via the `AgentAddress` enum (currently only `TmuxPane { pane_id, label }`).

## Paused messaging

While messaging is paused (`SetMessagingPaused`, see [Operation](../operation/README.md#pausing-messaging)) nothing on this page happens:

- **Listeners** — fetched iMessage rows and Telegram updates still advance the ROWID cursors and the update offset, but no `InboundMessageReceived` event is appended. Messages that arrive while paused are discarded and are not replayed on resume.
- **Routing** — an `InboundMessageReceived` delivery that was already in the outbox is marked as an intentional skip before its payload is read. No pane is discovered, nothing is sent to tmux and no confirmation goes out.
- **Replies** — the confirmation and error replies described under [Delivery](#delivery) are not sent.

## Pane discovery

Harold currently recognizes a pane as an agent when the pane process or one of its descendants has a process command containing a configured fragment:

- Shipped `[[agents]]` providers match `codex`, `claude`, and `opencode` command fragments
- Claude Code example: tmux may report `pane_current_command` as a Node-version-like process, but the pane's descendant command includes `claude`
- Codex example: tmux may report `pane_current_command` as `codex-aarch64-a`, while the pane's descendant command includes `codex`

This is a process-name heuristic. Update `agents[].command_contains` if an agent binary name changes.

Pane label format: `<session_name>:<window_index>.<pane_index>` (e.g. `alir-app main:0.1`).

## Routing resolution

```
route_inbound_message(text)
│
├─ parse_tag(text) → ([tag], body)
│
├─ tag present?
│   ├─ exact match on pane label → use it
│   └─ substring match (case-insensitive) → use it
│       └─ no match → return None (error iMessage)
│
├─ no tag → semantic_resolve(body, panes)
│   ├─ only 1 pane → skip (returns None, falls through)
│   └─ multiple panes → AI CLI (Sonnet, --max-turns 1, disableAllHooks)
│       prompt asks: "does this message have EXPLICIT routing intent?"
│       ├─ response = "none" → return None
│       └─ response = LINE1: pane label / LINE2: cleaned message → match by label
│
├─ last_away_notification_source_agent → find AgentAddress in live panes
│
└─ my-agent fallback → find pane whose label contains "my-agent"
```

## Delivery

Once a pane is resolved:

1. `is_pane_alive(pane_id)` — re-checks `tmux display-message -t <pane_id> -p #{pane_pid}` and the descendant process table to confirm the pane still hosts a known agent process
2. `strip_control(text)` — removes ANSI escape sequences and non-newline control characters
3. `tmux send-keys -t <pane_id> -l "📱 <body>"` — sends text literally (no shell interpretation)
4. `tmux send-keys -t <pane_id> Enter` — submits the message
5. Confirmation sent back via the configured away channel: `"✓ Delivered to [<pane_label>]"`. On iMessage the confirmation, like every error reply, is sent with the `🤖` prefix (`"🤖 ✓ Delivered to [<pane_label>]"`): a message to your own number is stored as both a sent and a received row, the listener reads both, and it skips only `🤖`-prefixed text, so an unprefixed confirmation would be routed back in as a new reply and confirmed again without end

If either tmux `send-keys` command fails, the outbox delivery remains pending for retry. A confirmation failure after tmux accepts the message is logged but does not retry the agent delivery, which avoids duplicating the user's input. If no pane is found, an error message listing the currently available pane labels is sent back via the away channel and the event is recorded as intentionally skipped.

## Semantic routing prompt

The AI CLI is invoked with Sonnet (`--max-turns 1`, `--settings '{"disableAllHooks":true}'`) with this prompt structure:

```
You are a routing classifier. Do NOT answer or respond to the message content.

MESSAGE TO CLASSIFY:
<message>
<body (with </message> tags stripped for injection prevention)>
</message>

ACTIVE TMUX PANES:
- <label1>
- <label2>

Pane labels use hyphens where users may write spaces (e.g. 'my agent' refers to 'my-agent').
Does the message contain EXPLICIT routing intent to a specific pane?
(direct address like 'To X,', 'ask X', '[X]', 'my agent')
If yes, reply on two lines:
LINE1: exact pane label
LINE2: message with routing prefix removed
If no explicit routing intent, reply: none
```

The message body is wrapped in `<message>` tags with `</message>` occurrences stripped to prevent prompt injection. The cleaned message from LINE2 is what gets relayed to the pane, stripping any routing prefix the user included.

## Sequence

```mermaid
sequenceDiagram
    participant Phone
    participant Channel as Away channel<br/>(iMessage or Telegram)
    participant Listener
    participant Store as Event store
    participant Handler as Event handler
    participant Tmux as tmux
    participant AiCli as claude (Sonnet)

    Phone->>Channel: User reply arrives
    note over Listener: iMessage: poll chat.db via FSEvents/5s fallback<br/>Telegram: long-poll getUpdates (30s)
    Channel-->>Listener: new message(s)
    Listener->>Store: append InboundMessageReceived { text }
    Listener->>Listener: advance cursor (atomic store, only on successful append)

    Handler->>Store: stage event and checkpoint in one transaction
    Store-->>Handler: pending InboundMessageReceived delivery

    Handler->>Tmux: list-panes -a -F "#{pane_id}|#{session_name}:#{window_index}.#{pane_index}|#{pane_pid}"
    Tmux-->>Handler: pane rows
    Handler->>Handler: filter rows where pane process tree contains a configured agent command fragment

    Handler->>Handler: parse_tag(text) → ([tag], body)

    alt [tag] present
        Handler->>Handler: exact label match, then case-insensitive substring match
    else no tag, multiple panes
        Handler->>AiCli: routing prompt with body + pane label list (--model sonnet --max-turns 1)
        AiCli-->>Handler: "none" or LINE1: label / LINE2: cleaned message
        Handler->>Handler: match returned label to live panes
    else fallback
        Handler->>Handler: find last_away_notification_source_agent in live panes
        Handler->>Handler: else find pane label containing "my-agent"
    end

    Handler->>Tmux: display-message -t pane_id -p #{pane_pid} → process-tree liveness check
    Handler->>Handler: strip_control(body) → remove ANSI + control chars
    Handler->>Tmux: send-keys -t pane_id -l "📱 <body>"
    Handler->>Tmux: send-keys -t pane_id Enter
    Handler->>Channel: "✓ Delivered to [pane_label]"
    Handler->>Store: mark delivery complete
```
