# 38. Harold reads ClickHouse for the evidence check, and writes the evidence and confirmed files

Date: 2026-11-09
Status: Accepted. Extends 0037 and 0021.

## Context

All work must be measurable. Each spec names evidence queries, and something must run them after the release and record the result. ADR 0037 allows only the factory flow query, and ADR 0021 keeps Harold out of the pod folder.

## Decision

- Harold also reads ClickHouse for the evidence check, with the same user, settings, limits and loopback rule as ADR 0037. The SQL is the text of the named files in `docs/references/telemetry/queries/evidence/` on each product's base branch, read with `git show`. The only value from outside the file is `since`, which Harold sends as a typed query parameter, never in the text.
- The bounds that the code enforces:
  - A query file is at most 64 KiB. A larger file gives `bad_query`.
  - The query name is the stem of an `<id>-<n>.sql` file that `git ls-tree` lists on the base branch. No spec text and no request text builds the path.
  - The password rule of ADR 0037 applies. The password stays in memory for one request, Harold reads it from `users_file` at each read and drops it after, and it is never in a setting, a log line, a span, a file or a fixture.
  - Harold accepts only one `JSONEachRow` row, with `present` 0 or 1 and `reason` of 1 to 40 characters from `a` to `z` and `_`. Any other body gives `bad_row`. So no query text and no free text reaches a file or a log.
- The evidence tool writes `evidence/<id>.md` and `confirmed/<id>.md` in the pod folder. This is an exception to ADR 0021, where Harold never writes in the pod folder. The evidence tool is the only part of Harold that writes there, and it writes no other file there.
- Each rewrite of an evidence file keeps the lines that a later step added to it, such as `told:` (evidence-follow-up).

## Consequences

- A merged evidence query runs with `readonly=1` as `hyperdx_ro`, so a wrong query can fail or read too much, but it cannot write.
- The review of the merge is the review of the query. An unmerged file never runs.
- `clickhouse_url` and `users_file` are still the `[factory_flow]` settings, so the loopback check at start covers both readers.
- A new reader of ClickHouse, or a new file that Harold writes in the pod folder, needs a new ADR.
