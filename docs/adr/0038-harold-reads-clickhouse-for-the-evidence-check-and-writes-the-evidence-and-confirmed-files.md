# 38. Harold reads ClickHouse for the evidence check, and writes the evidence and confirmed files

Date: <date of the merge>
Status: Accepted. Extends 0037 and 0021.

## Context

All work must be measurable. Each spec names evidence queries, and something must run them after the release and record the result. ADR 0037 allows only the factory flow query, and ADR 0021 keeps Harold out of the pod folder.

## Decision

- Harold also reads ClickHouse for the evidence check, with the same user, settings, limits and loopback rule as ADR 0037. The SQL is the text of the named files in `docs/references/telemetry/queries/evidence/` on each product's base branch, read with `git show`. The only value from outside the file is `since`, which Harold sends as a typed query parameter, never in the text.
- The evidence tool writes `evidence/<id>.md` and `confirmed/<id>.md` in the pod folder. This is an exception to ADR 0021, where Harold never writes in the pod folder. The evidence tool is the only part of Harold that writes there, and it writes no other file there.
- Each rewrite of an evidence file keeps the lines that a later step added to it, such as `told:` (evidence-follow-up).

## Consequences

- A merged evidence query runs with `readonly=1` as `hyperdx_ro`, so a wrong query can fail or read too much, but it cannot write.
- The review of the merge is the review of the query. An unmerged file never runs.
- A new reader of ClickHouse, or a new file that Harold writes in the pod folder, needs a new ADR.
