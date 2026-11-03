# 31. Agents record signals through Harold under a prefix

Date: 2026-11-04
Status: Accepted. Relies on ADR 0002 (local callers are trusted).

## Context

The chief and the PnEMs need to record what they measure, and today only Harold's code can.

## Decision

- A tool takes declared counters, gauges and events from local panes, under a prefix that the caller's pane allows.
- The tool exports them as `agent.*` through Harold's exporter.
- An event carries declared values only, never free text.

## Consequences

- A new signal needs no Harold change.
- The prefix check is a mix-up check. The pane comes from `TMUX_PANE` or `REX_BLOCK`, so a local process that lies can write any prefix (ADR 0002).
- Series stay bounded by the declared values and the caps, and writes by the rate limit.
