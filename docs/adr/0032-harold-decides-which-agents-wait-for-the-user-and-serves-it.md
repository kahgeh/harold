# 0032. Harold decides which agents wait for the user and serves it

Date: 2026-11-04
Status: Accepted (waiting-state-served). Builds on [0002](0002-tool-services-trust-local-callers.md) and [0027](0027-harold-reads-agents-on-other-rex-hosts-read-only-through-rex.md).

## Context

Each control-tower kept its own list of waiting agents. It read the focused pane itself every second. Two control-towers, or a restarted one, disagreed about the ◆ marks. A dashboard could not count the waiting agents. The remote control lost its `n` key for this reason.

## Decision

Harold decides which agents wait. It reads the focused pane every second and applies the rule that the control-tower used: an agent waits when its state goes from `working` or `blocked` to `done`, `idle` or `error` while its pane is not known to be the focused pane. An agent stops waiting when it is seen, when it works again, or when its pane goes.

Harold serves the result in `AgentPaneState` (`waiting`, `waiting_since_ms`) and `AgentStateSnapshot` (`focus_known`). A change sends a snapshot at the same event version.

When the focus is unknown, an agent that finishes waits, and an unknown focus never clears a mark. A control-tower that jumps to a pane calls `AgentSeen { pane_id }` on loopback. The call clears the mark of that pane and records the time waited. It does nothing for a pane that is not waiting or not known.

A new control-tower keeps its own rule when the snapshot has no `focus_known` (an older Harold). That fallback goes away in a later change.

## Consequences

- Every control-tower and the remote control show the same marks. `n` and `N` walk the same agents.
- Harold exports `harold.agents.waiting` and `harold.agent.waiting.duration`.
- `AgentSeen` is a new write call. Under [ADR 0002](0002-tool-services-trust-local-callers.md) local callers are trusted, so a local process can clear a mark. The call can change nothing else: it never approves or answers.
- An agent on a Rex host (ADR 0027) waits until it is seen by `AgentSeen` or works again, because Harold cannot read its focus.
- The list is in memory and starts empty after a Harold restart.
- Harold must read the focus: Rex with one session in the app, or the tmux active pane. The focus is unknown with two or more Rex sessions, an unreachable Rex, or no tmux client.
