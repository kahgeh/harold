# Away Channel Security: iMessage vs Telegram

Harold supports two away channels for notifications and inbound message routing when the screen is locked. Each has different security properties.

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

## Telegram

**Transport security:** Client-to-server encrypted (TLS), but **not end-to-end encrypted** by default. Telegram servers can read bot API message content. Secret chats (E2EE) are not available for bots.

**Authentication:** Requires a bot token (long-lived API credential) and chat ID. The bot token grants full control over the bot — anyone with the token can send/receive messages.

**Attack surface in Harold:**

- The bot token is stored in config files. If leaked, an attacker can impersonate Harold or read all messages sent to the bot.
- Inbound messages are filtered by `chat_id` — only messages from the configured chat are processed. Messages from other chats are ignored.
- The same tmux relay protections (literal mode, control character stripping) apply.
- Uses `reqwest::blocking::Client` with a 30s timeout. The `OnceLock` pattern prevents the client from being dropped inside an async context.

**Limitations:**

- Requires internet access for both sending and receiving (long-polling the Bot API).
- Telegram's servers are a single point of trust — they see all message content.
- Long-polling introduces latency compared to iMessage's filesystem-watch approach.

## Recommendation

Use **iMessage** when possible — it provides E2EE with no credentials to manage or leak. Choose **Telegram** when you need cross-platform access (e.g. notifications on a non-Apple device) and accept the trade-off that message content is visible to Telegram's servers and that a bot token must be protected.

In either case, Harold's inbound message filtering (handle IDs for iMessage, chat ID for Telegram) ensures that only messages from the expected conversation are processed and relayed to agent sessions.
