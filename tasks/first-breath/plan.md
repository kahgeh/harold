# First Breath — Implementation Plan

Get Harold working end-to-end, replacing the current Python relay (`imessage_listener.py`) and slimming down `smart_stop.py`.

---

## Step 1 — Configuration

No hardcoded values — all tunables loaded from config files and env vars.

- Add dependencies to `Cargo.toml`:
  - `config = "0.15.19"` — layered config (TOML + env vars)
  - `dotenvy = "0.15.7"` — `.env` file support
  - `serde` (with `derive`) — config struct deserialization
- Create `config/default.toml` with all defaults:
  - gRPC listen address/port
  - iMessage recipient
  - chat.db path
  - AI CLI path
  - TTS command
  - Log level
- Create `src/settings.rs` with:
  - `Settings` struct covering all config values
  - `Settings::load()` — layered: `default.toml` → `{env}.toml` → `HAROLD__` env vars
  - `OnceLock<Arc<Settings>>` global singleton
- Environment controlled via `HAROLD_ENV` (default: `local`)

---

## Step 2 — Logging

Set up structured logging from the start so all subsequent steps emit useful output.

- Add dependencies to `Cargo.toml`:
  - `tracing = "0.1.44"`
  - `tracing-subscriber = "0.3.22"` (with `env-filter`, `fmt` features)
  - `tracing-log = "0.2.0"`
- Create `src/telemetry.rs`:
  - `init_telemetry()` — sets up `tracing-subscriber` with JSON formatter and env filter
  - `TelemetryGuard` — ensures flush on shutdown
- Call `init_telemetry()` after config is loaded in `main()`
- Log level sourced from `Settings` (configurable via `HAROLD__LOG__LEVEL` env var)

---

## Step 3 — Proto definition

Define the gRPC service contract.

- Create `proto/harold.proto`
- Define `TurnComplete` RPC and `TurnCompleteRequest` / `TurnCompleteResponse` messages
- Add `tonic` and `tonic-build` to `Cargo.toml`
- Wire up `build.rs` to generate Rust from proto

---

## Step 4 — gRPC server skeleton

Stand up a minimal Harold binary that accepts `TurnComplete` calls.

- Add `tokio` async runtime
- Implement `HaroldService` with `TurnComplete` handler (stub — just log for now)
- Listen on `localhost:50051`
- Confirm `grpcurl` can call it

---

## Step 5 — Event store

Persist `TurnComplete` events using the events crate pattern.

- Add `events` as a git submodule (github.com/kahgeh/events)
- Define `TurnCompleted` domain event
- On RPC received → append event to store
- Wire up projector to consume events

---

## Step 6 — Notification: TTS (at desk)

Implement voice announcement for at-desk use.

- Screen lock detection via `ioreg`
- If screen unlocked → shell out to `say` (macOS) with summary
- Summary generated via AI CLI subprocess call
- Confirm voice fires on `TurnComplete` RPC

---

## Step 7 — Notification: iMessage (away)

Implement iMessage notification for away use.

- If screen locked → generate detailed summary via AI CLI
- Send iMessage via AppleScript
- Dedup: skip if last outgoing message is identical
- Split trailing question into a second message

---

## Step 8 — Reply routing

Poll `chat.db` and route incoming iMessage replies to tmux panes.

- Port filesystem watcher + chat.db polling from `imessage_listener.py`
- Implement routing:
  1. `[tag]` → exact/substring match against live tmux panes
  2. No tag → semantic resolve via AI CLI
  3. Fallback → `last_notified_pane`
  4. Final fallback → pane containing `my-agent`
  5. No match → error iMessage back
- `tmux send-keys` to relay message to pane
- Update `last_notified_pane` on successful notification

---

## Step 9 — Slim down smart_stop.py

Replace the current hook with a thin emitter.

- Strip out: summarisation, TTS, iMessage sending, registry writes
- Keep: transcript extraction, pane info, git context, SubagentStop skip
- Add: ensure Harold is running (start if not), call via `grpcurl`
- Confirm end-to-end flow works

---

## Step 10 — Cleanup

- Remove `imessage_listener.py` (replaced by Harold)
- Remove dead code from `smart_stop.py`
- Update `~/.claude/settings.json` Stop hook command if needed
- Confirm listener restart is no longer needed manually

---

## Definition of done

- [ ] Turn completes in Claude Code → voice fires at desk
- [ ] Turn completes with screen locked → iMessage received on phone
- [ ] Reply from phone → routed to correct tmux pane
- [ ] Multiple sessions → correct session receives reply
- [ ] Harold auto-starts if not running when hook fires
