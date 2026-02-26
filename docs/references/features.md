# Harold Features

## Notification (outbound)

| Feature | Detail |
| --- | --- |
| At-desk TTS | Speaks a short AI-generated summary via a configurable TTS command when the screen is unlocked |
| Away iMessage | Sends a detailed AI-generated summary via iMessage when the screen is locked |
| Question splitting | A trailing question in the summary is sent as a separate follow-up message |
| Duplicate suppression | Skips iMessage if the last outgoing message to the recipient is identical |
| Session skip | Skips notification entirely if the user is already in the active tmux session (`skip_if_session_active`, default on) |
| Local model support | Short TTS summaries can use a local model (`mlx_lm`) instead of the AI CLI |

## Reply routing (inbound)

| Feature | Detail |
| --- | --- |
| Tag routing | `[tag]` prefix in reply → exact then substring match against live tmux pane labels |
| Semantic routing | No tag, multiple panes → AI CLI determines routing intent from message content |
| Last-pane fallback | Falls back to the pane most recently notified |
| Default pane fallback | Final fallback to the pane whose label contains `my-agent` |
| Error reply | Sends an iMessage back listing available panes when no route is found |
| Delivery confirmation | Sends an iMessage confirmation after successfully relaying to a pane |

## Operation

| Feature | Detail |
| --- | --- |
| Auto-start | Agent stop hook starts Harold if not already running |
| Graceful shutdown | SIGINT/SIGTERM drains in-flight tasks then checkpoints the WAL |
| Diagnostics mode | `--diagnostics [--delay N]` tests screen lock detection, TTS, and iMessage config |
| Layered config | `default.toml` → `{env}.toml` → `HAROLD__*` environment variable overrides |
| Config auto-discovery | Config directory defaults to `config/` next to the binary; no env vars required |
