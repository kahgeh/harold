# How to set up Telegram as the away channel

This guide switches Harold's away-mode notifications and inbound message routing from iMessage to Telegram. Once configured, all away notifications, reply confirmations, and file readout responses go through a Telegram bot.

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

For an installed service, put these settings in `~/bin/harold/config/local.toml`. launchd starts the service, so Harold overrides in your shell never reach it.

The `[imessage]` section can remain — it is ignored when `away_channel = "telegram"`.

## 4. Restart Harold

If Harold is already running, restart it to pick up the new config:

```bash
~/bin/haroldctl restart
```

Use `~/bin/haroldctl status` to verify readiness without sending a message.

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
2. Trigger an agent turn completion from a configured Claude, Codex, or other Harold hook
3. Check Telegram for the notification
4. Reply in Telegram — verify the reply routes to the correct agent pane
5. Check the agent pane received your message prefixed with the phone emoji

## Several Macs: a hub and members

Telegram lets only one process read a bot's replies. With Harold on several Macs, one Mac, the hub, reads the replies and forwards each one to the Mac that should receive it. The other Macs, the members, do not read Telegram. A reply with no tag goes to the Mac that sent the latest notification. Skip this section if you run Harold on one Mac.

This path has unit tests only. It has not been run live on two Macs.

The examples use two Macs: `studio` is the hub and `laptop` is a member.

1. Choose the hub.
2. On every Mac, set `machines` to the names of all your Macs and, if the hostname is not the name you want, `machine_name`. Use the same `[telegram]` settings and `away_channel = "telegram"` on every Mac. See [Several Macs on iMessage](setup.md#several-macs-on-imessage) for the name rules.
3. On every Mac, set `[grpc] host` to that Mac's Tailscale address. The default, `127.0.0.1`, cannot be reached from the other Mac. Restart Harold on every Mac after you change it.

   Harold then listens only on that address, not on `127.0.0.1`. Local clients that default to `127.0.0.1` or `localhost` stop reaching it. Start the dash with `tmx-agent-dash --endpoint http://<tailscale-ip>:50060`, and set `HAROLD_ADDR=<tailscale-ip>:50060` for the OpenCode plugin. The Claude hooks keep working, because they read the address from the service metadata.
4. Generate one secret for each member. The hub and that member share it:

   ```bash
   openssl rand -hex 32
   ```

5. On the hub, add one `[[peers.members]]` block for each member. Each `name` is in `machines`.

   ```toml
   [grpc]
   host = "100.64.0.1"

   [[peers.members]]
   name = "laptop"
   url = "http://laptop.tailnet:50060"
   secret = "a long random string, the same on the hub and on this member"
   ```

6. On each member, set the role and the hub's address:

   ```toml
   [grpc]
   host = "100.64.0.2"

   [peers]
   role = "member"

   [peers.hub]
   url = "http://studio.tailnet:50060"
   secret = "the same string as in the hub's block for this Mac"
   ```

7. Restart Harold on every Mac.

What to expect:

- A member does not poll Telegram. Only the hub does.
- A reply with no tag goes to the Mac that sent the latest notification. A tag such as `[laptop]` or `[laptop/harold main:0.0]` goes to that Mac.
- When the Mac that should receive a reply is off or unreachable, the hub sends `🤖 [studio] could not reach laptop` and does not route the reply itself.
- After the hub restarts, it keeps every untagged reply itself until the next notification from any Mac.

The calls between Macs are plain HTTP. Tailscale encrypts the traffic and limits who can reach the port. With `grpc.host` on a Tailscale address, the existing RPCs, which check no secret, are reachable from your tailnet; see the [security audit follow-up](../security-audit-2026-02-27.md#follow-up-2026-10-11-grpc-beyond-loopback-for-several-macs). Keep the secrets in `local.toml`, which the installer makes readable by you only. launchd starts the installed service, so a variable in your shell never reaches it, and `haroldctl` and the installer clear inherited Harold overrides for their own configuration and readiness checks. A variable set for all of launchd with `launchctl setenv` does reach the service, so do not put a secret there; the service's own seven variables, `HAROLD_LAUNCHD` among them, win on a clash.

The [routing reference](../references/inbound-message-routing/README.md#several-macs) describes the rules, settings and failure modes.

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
- **Inbound listener** long-polls Telegram `getUpdates` instead of watching `chat.db`. With several Macs, only the hub does this and a member starts no listener
- Messages from the bot (prefixed with the robot emoji) are filtered out, same as iMessage
- Only messages from the configured `chat_id` are accepted

The at-desk path (TTS) is unaffected — it always uses the local speaker regardless of channel.
