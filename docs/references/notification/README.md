# Notification

Notification notifies the user of completed agent turns, via voice when at the desk or the configured away channel (iMessage or Telegram) when away.

## Problem

AI agents finish turns silently. Without active monitoring you won't know a task is done until you look — which breaks flow at your desk and leaves agents idle when you're away.

## Architecture

Harold's event handler stages `TurnCompleted` events in its durable outbox and calls `notify()` in stream-version order. Successful deliveries and intentional skips are marked complete. TTS command, Telegram HTTP/configuration, and iMessage process failures remain pending for retry. Malformed payloads and unknown event types are recorded as terminal failures so one poison event cannot block later work. The notification path is chosen based on runtime checks: whether the completing pane's tmux session has an attached client, whether the completing pane is the one the user is looking at, and whether the screen is locked.

Summarisation uses different backends depending on the path:

| Path              | Summary backend                      | Max input                     | Output                                            |
| ----------------- | ------------------------------------ | ----------------------------- | ------------------------------------------------- |
| At desk (TTS)     | Local model (`mlx_lm`)               | 500 chars of last_user_prompt | 3–8 words, ≤20 tokens                             |
| Away (iMessage)   | AI CLI or 280-char truncation        | Full assistant_message         | `🤖 [machine_name/pane_label] body (context)` + trailing question |
| Away (Telegram)   | AI CLI or 280-char truncation        | Full assistant_message         | `🤖 [machine_name/pane_label] body (context)` + trailing question |

If the local model is not configured, the TTS summary falls back to `"Work complete"`.

## Decision flow

```
TurnCompleted event (pane_id from $TMUX_PANE in stop hook)
       │
       ▼
  notify()
  │
  ├─ remove the pane's mark (set when the pane sent a message with
  │   send-message-to-away-device); remember whether it was under 10 minutes old
  │
  ├─ same pane and prompt recorded in the last 30 seconds? → skip (return)
  │
  ├─ skip_if_session_active = true?
  │   └─ tmux display-message -t <session> -p #{session_attached}
  │      attached ≠ 0 and screen unlocked → skip (return)
  │
  ├─ skip_if_pane_active = true?
  │   └─ ioreg → screen unlocked?
  │      tmux display-message -t <session> -p #{pane_id} → active pane
  │      active pane == completing pane → skip (return)
  │
  ├─ ioreg → IOConsoleLocked = true?
  │   ├─ no  → notify_at_desk()
  │   └─ yes → messaging paused? → skip (return)
  │           mark was under 10 minutes old? → leave out (return)
  │           away_updates_only_from set, an agent in that folder,
  │             and this pane outside it? → leave out (return)
  │           channels::notify_away() (iMessage or Telegram per config)
```

`<session>` is resolved from the completing pane via `tmux display-message -t <pane_id> -p #{session_name}`.

## At-desk: TTS

1. `build_short_summary()` — runs `uv run mlx_lm.generate` in `ai.local_model_dir` with a system prompt asking for a 3–8 word completion summary; strips `<think>...</think>` blocks from reasoning models
2. Message assembled: `"<summary> on <main_context> and waiting for further instructions"`
3. TTS command run: `<tts.command> [tts.args...] [-v tts.voice] "<message>"`
4. If the primary command fails to start or exits non-zero, Harold runs the fallback command when configured: `<tts.fallback_command> [tts.fallback_args...] [-v tts.fallback_voice] "<message>"`

Config keys (`[tts]`):

| Key                | Description                                               |
| ------------------ | --------------------------------------------------------- |
| `command`          | TTS binary (e.g. `say`)                                   |
| `voice`            | Optional voice name passed as `-v`                        |
| `args`             | Optional extra args prepended before `-v` and the message |
| `fallback_command` | Optional backup TTS binary when `command` fails           |
| `fallback_voice`   | Optional fallback voice name passed as `-v`               |
| `fallback_args`    | Optional fallback args prepended before `-v` and message  |

### Muting voice

`SetTtsMuted(muted)` turns the at-desk TTS channel off or on. The flag is stored as a `TtsMuteChanged` stream event and projected into the `settings` table, so it persists across daemon restarts until changed back. It is part of the agent-state snapshot (`tts_muted`), which is how `tmx-agent-dash` shows `VOICE ON` / `VOICE MUTED`.

- Mute gates only the at-desk TTS channel. The delivery is recorded as an intentional skip. Away notifications (iMessage, Telegram) are unaffected; they have their own switch, described below.
- A `TurnCompleted` delivered in the same projection batch as a mute change observes the post-batch value.
- Limitation: `SetTtsMuted` compares the requested value against the daemon's last published snapshot, so two opposite requests from a non-dash client within the projection window may drop the second. The dash always sends from confirmed state and is unaffected.

### Pausing messaging

`SetMessagingPaused(paused)` is the counterpart for the away channel. While paused, a `TurnCompleted` that would go to iMessage or Telegram is recorded as an intentional skip before anything is summarised or sent, and it is not sent later. At-desk TTS is unaffected, just as the voice mute does not affect the away channel. The flag is stored as a `MessagingPausedChanged` stream event, projected into the `settings` table, and published in the snapshot as `messaging_paused`. It also stops inbound routing and replies; see [Operation](../operation/README.md#pausing-messaging).

## Away: iMessage or Telegram

The away channel is selected by `[notify] away_channel` (`"imessage"` or `"telegram"`). Both channels share the same notification flow and prefix all outgoing messages with `🤖`. This serves two purposes:

- **Inbound filtering** — the listener skips messages starting with `🤖` so Harold doesn't route its own notifications back to agents as user replies
- **Visual distinction** — on your phone you can immediately tell which messages are from Harold vs from you

### Common steps (both channels)

1. `summarise_for_notification()` — uses AI CLI to summarise `assistant_message` into 2–3 sentences under 280 chars; falls back to `truncate_body()` (first 280 chars, newlines flattened)
2. `split_body()` — splits the last sentence ending in `?` into a separate follow-up message
3. Message assembled: `🤖 [<machine_name>/<pane_label>] <main body> (<main_context>)`. `machine_name` is this Mac's name, which defaults to the output of `hostname -s`.
4. Trailing question (if present) sent as a second `🤖`-prefixed message, `🤖 <question>`, without a tag

Confirmations and error replies carry the machine name too: `🤖 ✓ Delivered to [<machine_name>/<pane_label>]` and `🤖 [<machine_name>] <detail>`. The machine name tells you which Mac a notification came from and lets a reply reach that Mac. See [Several Macs](../inbound-message-routing/README.md#several-macs).

### iMessage-specific

5. Duplicate check — queries `chat.db` for the most recent outgoing message to first configured handle ID; skips if identical (after stripping `🤖` prefix)
6. Messages sent via AppleScript: `tell application "Messages" to send "🤖 ..." to buddy "..."`

Config keys (`[imessage]`):

| Key          | Description                                                          |
| ------------ | -------------------------------------------------------------------- |
| `recipient`  | Phone number or email of the iMessage recipient                      |
| `handle_ids` | All `chat.db` handle IDs for your Apple ID (dedup and inbound poll)  |

### Telegram-specific

5. Before the notification is sent, the Mac records which Mac sent the latest notification, so that a reply with no tag can be routed to it. The hub records itself as the latest notifier. A member calls `ReportNotification` on the hub with its machine name and the shared secret. A failed report is logged and the notification is still sent. On iMessage nothing is reported: the other Macs read the notification's row in `chat.db`.
6. Messages sent via Telegram Bot API `sendMessage` to the configured chat ID

Config keys (`[telegram]`):

| Key         | Description                                |
| ----------- | ------------------------------------------ |
| `bot_token` | Telegram bot token from @BotFather         |
| `chat_id`   | Numeric chat ID for the notification chat  |

## Updates Harold leaves out while you are away

With the screen locked, every agent's finished turn sends an update to the phone. Two rules leave an update out. Both apply only to an update that would go to the phone: the spoken update at the desk is given for every pane.

### After a message the agent sent itself

An agent that runs `harold send-message-to-away-device` and then ends its turn would put two messages on the phone: its own, and Harold's summary of a turn that only reports the message was sent. Harold sends the agent's message and leaves the update out.

When Harold sends an agent's message, it records the pane and the time. The pane's next `TurnCompleted` removes the record, and if the record is under 10 minutes old the update is left out with the log line `notification skipped (pane messaged the phone this turn)`. The record affects one turn: the pane's following turn sends its update. The 10-minute limit is there because Harold is not told about every turn end (Claude Code runs no hook for a turn you interrupt), and without it an old record would hide the update of a later turn. The record is kept in memory, so after a daemon restart between the message and the end of the turn the update is sent.

### From agents outside one folder

When one agent reports to the phone for the others, name the folder it runs in:

```toml
[notify]
away_updates_only_from = "/Users/you/Dev/chief-of-pods"
```

While an agent Harold tracks is running in that folder, only panes in that folder send a turn-end update. Every other pane's update is left out with the log line `notification skipped (updates come from <folder> only)`. While no agent is running in the folder, every pane sends its update as before.

| Key | Description |
| --- | --- |
| `away_updates_only_from` | Optional. An absolute path. Harold compares it as text with the working directory tmux reports for a pane (`pane_current_path`), ignoring one trailing `/`. It does not resolve symbolic links, and a subfolder counts as another folder. A value that is empty or does not start with `/` stops the daemon at start. |

- A message an agent sends with `harold send-message-to-away-device` is always sent, whichever folder the agent is in.
- The rule covers every turn of a pane outside the folder, including one you started in that pane yourself.
- Each Mac's Harold checks its own panes. An agent in the folder on another Mac is not seen.
- While the agent monitor cannot list tmux panes, the rule leaves nothing out.

### What leaving an update out means

Harold sends nothing to the phone for that turn: no summary and no trailing question. The event is marked handled and is not retried. The pane does not become the one an untagged reply goes to. The turn is recorded in the 30-second duplicate check, so a repeat of the same `TurnCompleted` is left out as well. The `TurnCompleted` event is still stored, so the dashboard and the pane's work summary update as usual.

## Messages an agent sends

A notification is Harold's summary of a finished turn. An agent can also send the user text of its own, at a moment it chooses, with one command:

```
~/bin/harold/harold send-message-to-away-device --src-id "$TMUX_PANE" 'Pod 3 is blocked on a merge conflict in store.rs'
```

The phone shows `🤖 [<machine_name>/<pane_label>] Pod 3 is blocked on a merge conflict in store.rs`. A reply to it is routed like a reply to a notification: an untagged reply goes back to the pane that sent the message.

The command calls the daemon's `SendMessageToAwayDevice` RPC and waits up to 3 seconds. The daemon makes the text one line, checks that it has 1 to 1,000 characters, that messaging is not paused and that the pane hosts an agent the monitor tracks, and then stores an `AwayMessageRequested` event. The command exits 0 once the event is stored, which is before the phone has the message. Otherwise it prints one line on standard error and exits 1. The event handler takes the event from the delivery outbox in stream-version order and sends it through the away channel.

It differs from a turn-end notification in these ways:

| | Turn-end notification | Agent message |
|---|---|---|
| Started by | The agent's turn ending | The agent running the command |
| Text | Harold's summary of the agent's final reply, 280 characters at most | The agent's text as written, made one line, 1,000 characters at most |
| Form | `🤖 [machine_name/pane_label] body (context)`, with a trailing question sent separately | `🤖 [machine_name/pane_label] text`, one message |
| Presence checks | Sent to the phone only when the screen is locked; skipped when the session or pane is active; left out by the two rules in "Updates Harold leaves out while you are away" | None: sent whether the screen is locked or not |
| Voice mute | Silences the at-desk path | Does not apply |
| Duplicate checks | The 30-second window, and on iMessage the recent-outgoing check | None: the same text sent twice arrives twice |
| Messaging pause | Recorded as an intentional skip | The command is refused with `messaging is paused`; a message already stored is skipped and not sent later |

A send that fails stays pending and is retried, as a failed notification is. The `send-message-to-away-device` skill, which the installer links for Claude Code and Codex, teaches an agent the command; see [setup](../../how-tos/setup.md).

## Sequences

### At desk

```mermaid
sequenceDiagram
    participant Hook as Stop hook
    participant gRPC as Harold (gRPC)
    participant Store as Event store
    participant Handler as Event handler
    participant Tmux as tmux
    participant LocalModel as mlx_lm
    participant TTS as TTS command

    Hook->>gRPC: TurnComplete RPC (pane_id, pane_label, last_user_prompt, assistant_message from hook input, main_context)
    gRPC->>Store: append TurnCompleted event
    Store-->>gRPC: ok
    gRPC-->>Hook: accepted: true

    Handler->>Store: stage event and checkpoint in one transaction
    Store-->>Handler: pending TurnCompleted delivery
    Handler->>Tmux: display-message -t <pane_id> -p #{session_name} → session
    Handler->>Tmux: display-message -t <session> -p #{session_attached} → attached?
    note over Handler: not attached → proceed
    Handler->>Handler: ioreg → IOConsoleLocked = false
    Handler->>LocalModel: system prompt + "User's last request: <last_user_prompt>" → ≤20 tokens
    LocalModel-->>Handler: "Fixed event handler shutdown"
    Handler->>TTS: say [-v Samantha] "Fixed event handler... on harold and waiting for further instructions"
    Handler->>Store: mark delivery complete
    note over Handler: at-desk does not update last_away_notification_source_agent
```

### Away (screen locked)

```mermaid
sequenceDiagram
    participant Hook as Stop hook
    participant gRPC as Harold (gRPC)
    participant Store as Event store
    participant Handler as Event handler
    participant Channel as Away channel<br/>(iMessage or Telegram)

    Hook->>gRPC: TurnComplete RPC (last_assistant_message from hook input)
    gRPC->>Store: append TurnCompleted event
    gRPC-->>Hook: accepted: true

    Handler->>Store: stage event and checkpoint in one transaction
    Store-->>Handler: pending TurnCompleted delivery
    Handler->>Handler: ioreg → IOConsoleLocked = true
    note over Handler: left out here if the pane messaged the phone this turn,<br/>or if updates come from another folder only
    Handler->>Handler: summarise_for_notification() via AI CLI (fallback: truncate to 280 chars)
    Handler->>Handler: split_body() → main body + trailing question (if ends in ?)
    Handler->>Channel: send "🤖 [studio/harold:0.3] <body> (harold)"
    note over Channel: iMessage: dedup check via chat.db then osascript<br/>Telegram: Bot API sendMessage
    Handler->>Handler: set last_away_notification_source_agent
    Handler->>Channel: send "🤖 <trailing question>" (if present)
    Handler->>Store: mark delivery complete
```
