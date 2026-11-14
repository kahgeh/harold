# 42. Harold reads three environment variables of an agent process

Date: 2026-11-14
Status: Accepted (agent-resource-use). Builds on 0004, 0040 and 0041.

## Context

A machine use alert named only the 3 busiest processes of one sample. It did not tell which agent started them. To give a process to an agent, Harold must know the role of each agent. ADR 0004 lets tools read Harold's agent list and pod facts. ADR 0040 keeps a record of the agent in each pod role. A record can be missing, and a build worker has no record in the form that this feature reads.

## Decision

- **The parent walk.** At each poll, Harold reads every process with its parent pid. It follows the parents up to the first live agent that the agent monitor knows, of any provider. It gives the process to that agent.
- **Three sources, in this order.** First, the pod role record, by pid and process start time, for `chief`, `pnem` and `pod_coordination` only. Second, the agent-pane file of the working directory of the agent's pane, which names a build worker. Third, the environment of the agent process.
- **The fallback reads the environment.** Harold reads the environment of an agent that the first two sources did not match. The agent can be of any provider. The spec said "a `claude` agent". This ADR does not name a provider, so that other harnesses can join. Harold reads the environment once for each pid and start time.
- **Only three variables are kept.** Harold keeps `POD_ROLE`, `POD_PRODUCT` and `POD_SPEC`, and drops every other variable at once. At the source, the reader of the process table returns only these three keys. `POD_ROLE` must be one of `chief`, `pnem`, `pod_coordination`, `build_worker` and `other`. `POD_PRODUCT` and `POD_SPEC` must hold at most 64 characters of `a-z`, `0-9` and `-`. A value that fails its rule is dropped, and the agent is `other`.
- **No environment value leaves Harold.** No value goes into a log line, a span, a metric attribute or an error text. A test with a secret value proves it.

## Consequences

- Harold reads data that belongs to another program. The ADR keeps the read to three names and to one read for each process.
- A new process of an agent that has no record and no `POD_ROLE` is `other` until a record exists.
- A child that leaves its parent chain, for example a daemon that `launchd` takes, is `none`.
- The agent-pane file and the `started/<id>.md` files are read, never written. The spec id of a build worker is `none` when the base checkout of its worktree is not found from the `.git` file.
