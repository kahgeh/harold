# 8. Harold reaches the phone by iMessage only, and its gRPC server stays on the Mac

Date: 2026-10-22
Status: Accepted (remove-telegram, merged at 69039bc). Amends 0002.

## Context

Harold supported Telegram as a second text channel. Telegram lets one process read a bot's replies, so two Macs needed a hub and member setup, with two peer RPCs and a shared secret, and a gRPC server listening on each Mac's Tailscale address. The user uses only iMessage, on both Macs, and the hub and member path never ran on two Macs. With iMessage, every Mac reads every reply from its own `chat.db` and decides ownership alone.

## Decision

- Harold texts the phone and reads its replies by iMessage only. Telegram, the hub and member setup, `ReportNotification` and `ForwardReply` are removed.
- `[grpc] host` must be a loopback address: `127.0.0.0/8`, `::1` or `localhost`. Harold's gRPC services trust local callers (ADR 0002), and nothing now needs another Mac to reach them.
- A config that still names Telegram, `[peers]`, `notify.telegram`, an `away_channel` other than `"imessage"` or a `grpc.host` beyond loopback is refused at install and at start, with the setting named and no value shown.

## Consequences

- A phone that is not an iPhone can reach Harold only through the web app.
- Two Macs need only the same Apple ID and the `machines` list. Nothing passes between them.
- ADR 0002's "peer calls" and the 2026-10-11 decision in the architecture file stay as records of their time.
- Adding another text channel later means deciding again how several Macs share its replies.
