# 4. Tools may read Harold's agent list and pod facts

Date: 2026-10-20
Status: Accepted (watch-jobs)

## Context

[ADR 0001](0001-tools-are-modules-hosted-by-harold.md) gives a tool only its data folder, typing, the chief-pane and pane-folder lookups, the products and the shutdown signal. The watch tool needs the agent states and the pod facts to hand a model what a pod check needs.

## Decision

`ToolContext` gains two read-only ports, `agents` and `pods`, which return copies of Harold's current agent list and pod stats' latest snapshot. A tool cannot change either through them.

## Consequences

- A tool can see every agent's folder and work summary, so it must not log them or send them anywhere its own spec does not allow.
- The core is still reached only through `ToolContext`.
