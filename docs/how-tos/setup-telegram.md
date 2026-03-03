# How to set up Telegram as the away channel

This guide switches Harold's away-mode notifications and reply routing from iMessage to Telegram. Once configured, all away notifications, reply confirmations, and file readout responses go through a Telegram bot.

## Prerequisites

- Harold already set up and working (see [setup.md](setup.md))
- A Telegram account

## 1. Create a Telegram bot

1. Open Telegram and search for **@BotFather**
2. Send `/newbot`
3. Choose a display name (e.g. `Harold`)
4. Choose a username (must end in `bot`, e.g. `my_harold_bot`)
5. BotFather replies with a **bot token** — copy it. It looks like `123456789:ABCdefGHIjklMNOpqrsTUVwxyz`

## 2. Get your chat ID

1. Open a chat with your new bot in Telegram and send any message (e.g. `hello`)
2. In a terminal, fetch the update to find your chat ID:

   ```bash
   curl -s "https://api.telegram.org/bot<YOUR_BOT_TOKEN>/getUpdates" | python3 -m json.tool
   ```

3. In the JSON output, find `result[0].message.chat.id` — this is your numeric chat ID (e.g. `12345678`)

If `result` is empty, make sure you sent a message to the bot first.

## 3. Configure Harold

Edit your `local.toml`:

```toml
[telegram]
bot_token = "123456789:ABCdefGHIjklMNOpqrsTUVwxyz"
chat_id = 12345678

[notify]
away_channel = "telegram"
```

Or use environment variables:

```bash
export HAROLD__TELEGRAM__BOT_TOKEN="123456789:ABCdefGHIjklMNOpqrsTUVwxyz"
export HAROLD__TELEGRAM__CHAT_ID=12345678
export HAROLD__NOTIFY__AWAY_CHANNEL="telegram"
```

The `[imessage]` section can remain — it is ignored when `away_channel = "telegram"`.

## 4. Restart Harold

If Harold is already running, restart it to pick up the new config:

```bash
pkill -f ~/bin/harold/harold
~/bin/harold/harold &
```

Or just let the stop hook auto-start it on the next agent turn.

## 5. Verify

Run diagnostics to confirm the channel is active:

```bash
~/bin/harold/harold --diagnostics
```

Expected output:

```
=== Harold diagnostics ===

screen_locked : false
away_channel  : telegram
iMessage      : recipient=+61... handle_ids=[36]
Telegram      : bot_token=(set) chat_id=12345678
TTS           : command=say voice=Some("Samantha")
AI cli        : "/usr/local/bin/claude"
```

To test the away path, lock your screen first:

```bash
~/bin/harold/harold --diagnostics --delay 10
```

Lock your screen during the 10-second window. You should receive a diagnostic notification in Telegram.

## 6. Test the full loop

1. Lock your screen
2. Trigger an agent turn completion (run a Claude Code command in a tmux pane)
3. Check Telegram for the notification
4. Reply in Telegram — verify the reply routes to the correct agent pane
5. Check the agent pane received your message prefixed with the phone emoji

## Switching back to iMessage

Set `away_channel` back to `"imessage"` in `local.toml` and restart Harold:

```toml
[notify]
away_channel = "imessage"
```

## How it works

When `away_channel = "telegram"`:

- **Outbound notifications** use the Telegram Bot API `sendMessage` instead of AppleScript/Messages.app
- **Reply confirmations** (delivered, error messages) are sent via Telegram instead of iMessage
- **File readout** uses `curl` to send voice messages via Telegram `sendVoice` instead of `osascript`
- **Inbound listener** long-polls Telegram `getUpdates` instead of watching `chat.db`
- Messages from the bot (prefixed with the robot emoji) are filtered out, same as iMessage
- Only messages from the configured `chat_id` are accepted

The at-desk path (TTS) is unaffected — it always uses the local speaker regardless of channel.
