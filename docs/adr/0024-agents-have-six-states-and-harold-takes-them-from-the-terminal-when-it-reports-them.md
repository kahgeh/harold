# 24. Agents have six states, and Harold takes them from the terminal when the agent reports them

Date: 2026-11-01
Status: Proposed (rex-state-from-rex). Builds on 0022.

## Context

Harold showed an agent as Busy, Idle or Unknown. Two states hid an agent that waits for a permission, one that finished a turn, and one that failed. On Rex, one screen read is three or more `rex` calls and can take more than 2 seconds. Claude Code writes its own state to its terminal with `OSC 7501` (the Program Status Protocol), and Rex keeps the last report for each block. The user decided that Harold's agent state is one set of states that no multiplexer owns.

## Decision

1. An agent's state is one of `working`, `blocked`, `done`, `idle`, `error` and `unknown`, in the API, the store, the events, the control-tower, the phone app and the glossary. Screen adapters and tmux give `working` or `idle`.
2. Harold reads a state word it does not know, in an event or a row, as `unknown` (effective state) or as no observation (hook or screen state).
3. Harold relies on the program status records that Rex keeps from `OSC 7501` reports. It uses only the root record of a block. A record counts only when its `owner.pid` is the agent's pid.
4. Harold keeps one long-lived `rex events -s <session> --json` child process for each Rex session. ADR 0022 allows only short calls. This is an addition for events only. A session with no stream gets one `program_status` call, then a screen read.
5. The record is a conclusive screen observation with the classifier id `rex_program_status`. The hook grace periods apply to it unchanged, with working or blocked read as working, and done, idle or error read as idle. `msg` is the current step: Harold keeps it in memory, sends it only to the control-tower, and does not treat it as a work summary or as new activity.
6. Code asks `is_working()` (working, blocked) or `is_at_rest()` (done, idle, error). No typing rule changes.
7. The installer copies `data/store` and `data/events` into the backup before the new Harold starts.

## Consequences

- A rollback to a Harold older than the new states needs the data copy, and loses what came after the install. A rollback between later stages needs no data copy, because of decision 2.
- An old control-tower shows UNKNOWN for blocked, done and error until it restarts.
- Claude Code agents in Rex get their state with no screen read. Codex and OpenCode keep the screen read.
- Harold runs one more child process for each Rex session, and drops a session's records when its stream ends.
- Harold depends on the shape of Rex's `program_status` and event output.
