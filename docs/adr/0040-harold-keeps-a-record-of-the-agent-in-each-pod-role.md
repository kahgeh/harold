# 40. Harold keeps a record of the agent in each pod role

Date: 2026-11-14
Status: Accepted (pod-role-record). Builds on 0001, 0002, 0004 and 0021.

## Context

The chief and Harold found a pod role by its folder or by a session name. Two chiefs run in one folder during a handoff. The PnEM and pod coordination share a folder. Claude Code makes a new session name at each restart.

## Decision

- **A hook reports each start.** A Claude Code `SessionStart` hook reports the agent of each pod role to Harold with the RPC `PodAgentStarted`. Harold keeps the newest agent for each (product, role) in `pod-roles.json` in the data folder of the pod tool. The pod roles are `chief`, `pnem` and `pod_coordination`. The product of the chief is `-`.
- **The pod tool answers `Who`.** The pod tool (`tools.pod`, a hosted tool under ADR 0001, local callers trusted under ADR 0002) answers `Who` from the record. It checks that the agent's process still runs with the same start time.
- **Harold reads one file of the agent.** Harold reads the agent's own file `~/.claude/sessions/<pid>.json`, and only the fields `pid`, `procStart` and `name`, to give a session name. This is a read of an agent's files under ADR 0004. A change of its shape gives `session_file_unreadable`, not a wrong name.
- **The record is the first source for the chief's pane.** `pick_chief` reads the record first. `notify.chief_session` and then the oldest session in the folder stay as the fallbacks, in that order.
- **Harold only reads the start file.** The start file `start/<role>.json` in the pod folder is written by the chief's `bin/pod` and `bin/up`. This adds one file that Harold reads to ADR 0021, and no file that Harold writes.

## Consequences

- A Claude Code release that changes the session file breaks `Who` until Harold reads the new shape. `bin/pod chief` then uses its folder scan, as before.
- The pod metrics keep `pod_pane_roles`, so two sources of roles exist until a later spec moves them to the record.
- A nested agent that inherits `POD_ROLE` from a role's shell replaces the record when it starts.
- A record with `agent_proc_start` 0 matches any live process with the same pid, so a reused pid reads as the agent.
- Build workers have no record. Pod coordination finds them by `.superpowers/baton/agent-pane`.
