# Pod Roles and the Restart Check

This page describes the agents that Harold tracks in a pod, and the check that Harold runs after one of them restarts. Stage 1 of the spec `agent-restart` checks two groups: identity and communication. Setup and memory come in stage 2.

## The agents

| Role | Role value | Record key |
| --- | --- | --- |
| chief | `chief` | `-/chief` |
| PnEM | `pnem` | `<product>/pnem` |
| pod-coordination | `pod_coordination` | `<product>/pod_coordination` |
| build worker | `build_worker` | `<product>/build_worker/<spec id>` |

The pod role record (`pod-roles.json`) holds one record for each key. The mocks of the check keep the word "worker" for a build worker. The code, the docs and the logs use "build worker".

## When the check runs

- At the end of the first turn of a restart. Harold sees a restart when a role start arrives for a role that already held another session. The hook sources `clear` and `compact` are no restart.
- On demand: `harold tool pod check <product> <role> [--spec <id>]`.

## Identity

Harold compares its record (pane and agent pid) with where the agent runs now. The pane comes from the `Stop` hook. The agent pid comes from the role start. The multiplexer must show that pid in that pane.

1. On a mismatch in the record, Harold corrects the record from the multiplexer's view and checks again. The row says `fixed`. A second mismatch is a FAIL.
2. For a PnEM and a pod-coordination, Harold also reads the `agent-panes` file of the pod. Line one is pod-coordination and line two is the PnEM. For a build worker, Harold reads the `agent-pane` file of its worktree.
3. Harold never changes these two files. A mismatch in a file stays a FAIL for the owner of the file.
4. When the multiplexer cannot be read, the row is `not checked` with the reason `multiplexer_unreachable`.

## Communication

Harold tests each agent that the role talks to, in both directions. Harold sends no message, because a message starts a turn and costs tokens. The test proves the address and the delivery point, not that the agent reads the message.

| Role | Talks to |
| --- | --- |
| chief | the PnEM and the pod-coordination of each running pod |
| PnEM | the chief and its pod-coordination |
| pod-coordination | the chief, its PnEM and each running build worker |
| build worker | its pod-coordination |

A direction passes when two tests pass:

1. The address resolves by the lookup that the sender uses, and the result is the agent that runs now. For a pod role the lookup is the record, with a check that the process runs (`harold tool pod who`). For a build worker it is the `agent-pane` file, with the multiplexer's view of that pane.
2. The delivery point accepts. The kind of the link sets the delivery point:
   - **Between pod roles** the link is a cross-session message (`SendMessage`). Harold connects to the messaging socket in the receiver's session file and closes it with no bytes sent.
   - **Between a build worker and its pod-coordination** the link is the pane (typed lines). The pane must exist and hold the receiver's agent pid.

The reason words are `no_record`, `agent_gone`, `agent_pane_file`, `pane_no_agent`, `old_agent`, `socket_missing`, `socket_refused`, `pane_holds_other_agent` and `multiplexer_unreachable`. A messaging socket that the session file does not name gives `not checked` with `socket_unknown`. A change of the session file shape gives the same result.

## The output and the receipt

The first line names the agent, its session label, its short pane id and the local time. The groups follow as plain headers: identity and communication. Each FAIL has one line under its table that names the fix. A line `not checked` carries its reason. The last line gives the count of FAIL and who gets them.

Harold writes the same text to the restart receipt file:

| Agent | File |
| --- | --- |
| chief | `<chief folder>/.superpowers/pod/restarts/chief-<YYYYMMDD-HHMM>.md` |
| PnEM, pod-coordination | `<pod folder>/restarts/<role>-<YYYYMMDD-HHMM>.md` |
| build worker | `<pod folder>/restarts/worker-<spec id>-<YYYYMMDD-HHMM>.md` |

A second run in the same minute replaces the file. The pod folder is `.superpowers/pod` in the working folder of the PnEM or of the pod-coordination. A build worker's receipt goes in the pod folder of its pod-coordination.

## Who gets a FAIL

- The chief gets the FAIL of a PnEM and of a pod-coordination.
- Pod-coordination gets the FAIL of its build workers.
- The chief's own FAIL goes to the phone as one line.

Harold types one line into the pane, as the writer `restart_check`, and only when the agent in the pane is idle. A busy pane gets nothing typed, and the receipt file keeps the text.

## Telemetry

See the [telemetry reference](../telemetry/README.md): the counter `harold.restart_check.results`, the log lines `restart check result` and `restart check delivered`, the span `harold.restart_check`, the query `restart-checks.sql` and the evidence query `agent-restart-1.sql`.
