# Away Channel Security: iMessage

Harold texts the phone and reads its replies by iMessage when the screen is locked. This page describes the security properties of that channel.

## iMessage

**Transport security:** End-to-end encrypted (E2EE) between Apple devices. Apple cannot read message content in transit or at rest on their servers.

**Authentication:** Tied to Apple ID. No bot tokens or API keys to manage or leak.

**Attack surface in Harold:**

- Inbound messages are filtered by `handle_id` — only messages from conversations matching configured handle IDs are processed. An arbitrary external sender's messages are never read.
- Messages are relayed to tmux panes via `tmux send-keys -l` (literal mode) with ANSI/control character stripping, preventing terminal escape injection.
- Outgoing messages pass through `sanitise_for_applescript` + backslash/quote escaping before entering AppleScript.

**Limitations:**

- macOS only — requires the Messages app and `osascript`.
- Relies on `sqlite3` CLI queries against `~/Library/Messages/chat.db`, which Apple could change without notice.
- Duplicate detection depends on reading outgoing message history from the same database.

## Handle filtering

Harold's inbound message filtering (handle IDs) ensures that only messages from the expected conversation are processed and relayed to agents.
