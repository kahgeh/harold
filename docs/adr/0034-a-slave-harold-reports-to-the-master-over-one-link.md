# 34. A slave Harold reports to the master over one link

Date: 2026-11-05
Status: Accepted. Amends ADR 0002 and ADR 0019.

## Context

The user runs agents on the Mac and on omarchy. Each machine runs its own Harold, and the two do not talk. The Mac's Harold sees omarchy's agents only by reading omarchy's Rex screens. It never sees their turn ends or their questions.

## Decision

The Mac's Harold is the master. The Harold on omarchy is a slave.

- The slave opens one gRPC stream to the master over the tailnet, `harold.slave.v1.SlaveLink/Open`, and keeps it open.
- The slave sends its agents (a `Snapshot`), its turn ends (a `Turn`) and its screen lock (a `Lock`).
- A paired slave tells the user nothing itself. Only the master delivers a turn end or a question.
- A slave pairing secret is the only check of a link. The master keeps one for each slave.
- There is no failover. When the link is down, the slave keeps working alone and the master reads the slave's agents through Rex again.

In 2c.2 the master will send the slave requests to type a line into a pane. The slave will type it and answer. The first slice, 2c.1, builds only the reports.

## Consequences

The user sees omarchy's agents and their questions from the Mac and from the phone.

The link listener is the one listener that does not trust local callers. `tailscale serve` hands each connection to the listener as a local caller, so the secret is the only check. This amends ADR 0002.

The machine that holds an agent decides at-desk or away. For an omarchy agent, the slave's last `Lock` decides. This amends ADR 0019.

A slave keeps each report it has not yet sent in `report-backlog.db`, for up to 24 hours or 1,000 reports. It then drops the oldest and counts them. The turn text stays on the slave's disk only for that time, and no log line holds it.

The recap text of an agent does not cross the link. The recap comes from the transcript, and the transcript path stays on the slave.

The path through `tailscale serve` is not tested until real use. The chief runs the one `tailscale serve` command. Harold never changes Tailscale.
