# First Breath — Implementation Plan

Get Harold working end-to-end, replacing the current Python relay (`imessage_listener.py`) and slimming down `smart_stop.py`.

---

## Step 1 — Proto definition

Define the gRPC service contract.

- Create `proto/harold.proto`
- Define `TurnComplete` RPC and `TurnCompleteRequest` / `TurnCompleteResponse` messages
- Add `tonic` and `tonic-build` to `Cargo.toml`
- Wire up `build.rs` to generate Rust from proto

---

## Step 2 — gRPC server skeleton

Stand up a minimal Harold binary that accepts `TurnComplete` calls.

- Add `tokio` async runtime
- Implement `HaroldService` with `TurnComplete` handler (stub — just log for now)
- Listen on `localhost:50051`
- Confirm `grpcurl` can call it

---

## Step 3 — Event store

Persist `TurnComplete` events using the events crate pattern.

- Add alir-platform `events` as a git submodule
- Define `TurnCompleted` domain event
- On RPC received → append event to store
- Wire up projector to consume events

---

## Step 4 — Notification: TTS (at desk)

Implement voice announcement for at-desk use.

- Screen lock detection via `ioreg`
- If screen unlocked → shell out to `say` (macOS) with summary
- Summary generated via AI CLI subprocess call
- Confirm voice fires on `TurnComplete` RPC

---

## Step 5 — Notification: iMessage (away)

Implement iMessage notification for away use.

- If screen locked → generate detailed summary via AI CLI
- Send iMessage via AppleScript
- Dedup: skip if last outgoing message is identical
- Split trailing question into a second message

---

## Step 6 — Reply routing

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

## Step 7 — Slim down smart_stop.py

Replace the current hook with a thin emitter.

- Strip out: summarisation, TTS, iMessage sending, registry writes
- Keep: transcript extraction, pane info, git context, SubagentStop skip
- Add: ensure Harold is running (start if not), call via `grpcurl`
- Confirm end-to-end flow works

---

## Step 8 — Cleanup

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
