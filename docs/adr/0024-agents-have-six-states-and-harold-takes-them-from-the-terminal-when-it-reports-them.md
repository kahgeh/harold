# 24. Agents have six states, and Harold takes them from the terminal when the agent reports them

Date: 2026-11-01
Status: Accepted (rex-state-from-rex). Builds on 0022.

## Context

Harold showed an agent as Busy, Idle or Unknown. Two states hid an agent that waits for a permission, one that finished a turn, and one that failed. On Rex, one screen read is three or more `rex` calls and can take more than 2 seconds. Claude Code writes its own state to its terminal with `OSC 7501` (the Program Status Protocol), and Rex keeps the last report for each block. The user decided that Harold's agent state is one set of states that no multiplexer owns.

## Decision

1. An agent's state is one of `working`, `blocked`, `done`, `idle`, `error` and `unknown`, in the API, the store, the events, the control-tower, the phone app and the glossary. Screen adapters and tmux give `working` or `idle`.
2. Harold reads a state word it does not know, in an event or a row, as `unknown` (effective state) or as no observation (hook or screen state).
3. Harold relies on the program status records that Rex keeps from `OSC 7501` reports. It uses only the root record of a block. A record counts only when its `owner.pid` is the agent's pid.
4. Harold keeps one long-lived `rex events -s <session> --json` child process for each Rex session. ADR 0022 allows only short calls. This is an addition for events only. An event is only a trigger: Harold calls `program_status` once for the block, and the answer replaces the block's record whole. Harold never builds a record from the event, because a real event has no `owner.pid`. At most one call runs at a time for each block, so a burst of events costs at most two calls. When the call fails, Harold drops the block's record. The block then has no usable record, so the observer makes one direct `program_status` call, and reads the screen only if that call also fails. A session with no stream gets the same direct call, then the screen read.
5. The record is a conclusive screen observation with the classifier id `program_status`. The hook grace periods apply to it unchanged, with working or blocked read as working, and done, idle or error read as idle. `msg` is the current step: Harold keeps it in memory, sends it only to the control-tower, and does not treat it as a work summary or as new activity.
   - Amended in stage 3: within the grace window, a blocked record of the agent's pid refines a working hook report, and an error record refines a done or idle report. Any other record still loses to the hook until the window ends. A hook report does not clear a record that refines it: only a new record replaces it. One reduction gives the state to every reader, the blocked timer included.
6. Code asks `is_working()` (working, blocked) or `is_at_rest()` (done, idle, error). No typing rule changes.
7. The installer copies `data/store` and `data/events` into the backup before the new Harold starts.

## Consequences

- A rollback to a Harold older than the new states needs the data copy, and loses what came after the install. A rollback between later stages needs no data copy, because of decision 2.
- An old control-tower shows UNKNOWN for blocked, done and error until it restarts.
- Claude Code agents in Rex get their state with no screen read. Codex and OpenCode keep the screen read.
- Harold runs one more child process for each Rex session, and drops a session's records when its stream ends.
- Each event costs one more `program_status` call (about 40 ms). This is the price of a correct pid. The calls that stage 2 removes from the scan must give a lower total of Rex calls.
- Harold depends on the shape of Rex's `program_status` and event output.
