# 41. Harold checks each agent after a restart

Date: 2026-11-14
Status: Accepted (agent-restart, stage 1). Builds on 0002, 0021, 0038 and 0040.

## Context

Agents restart after a crash, an update, a login or a machine restart. Nothing proves that the new agent is whole. ADR 0021 and ADR 0038 list the files that Harold writes in a pod folder. ADR 0040 keeps a record of the agent in each pod role, and it has no record of a build worker.

## Decision

- **A check after each restart.** After a restart of the chief, a PnEM, a pod-coordination or a build worker, Harold checks identity and communication. Harold writes the result to `restarts/<role>-<time>.md` in the pod folder. This extends ADR 0021, in the form of ADR 0038. Stage 2 adds the groups setup and memory.
- **Build workers are in the record.** The record has one entry for each (product, role, spec id). The spec id is empty for a pod role, and the role value `build_worker` has a spec id. This replaces the last line of ADR 0040.
- **The communication test sends no message.** Harold connects to the agent's messaging socket and closes it, with no bytes sent. The test proves the address and the delivery point. It does not prove that the agent reads a message.
- **Each FAIL goes to the agent that fixes it.** The chief gets the FAIL of a PnEM and a pod-coordination. Pod-coordination gets the FAIL of a build worker. Harold types the line into an idle pane. The chief's own FAIL goes to the phone.
- **Harold changes only the files that it owns.** After an identity mismatch, Harold corrects its own record. It never changes the `agent-panes` file or the `agent-pane` file.

## Consequences

- A change of the messaging socket in the agent's session file gives `not checked` with `socket_unknown`, not a wrong result.
- Each restart adds one small file to the pod folder. After approval, the chief adds `restarts/` to the standards' list of files that Harold may write in a pod folder.
- The memory check of stage 2 runs a hosted model and departs from ADR 0025 in a scope that the user approved. That decision lands with stage 2.
