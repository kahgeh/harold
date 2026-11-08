# 37. Harold reads ClickHouse only for the factory flow page

Date: 2026-11-08
Status: Accepted.

## Context

Harold sends its logs, traces and metrics to the collector, which stores them in ClickHouse. Until now, Harold never read ClickHouse. The factory flow page needs numbers that only ClickHouse holds: the items that wait in each flow stage and their median wait. The chief reads the same numbers with `bin/logq --named factory-flow`. Both readers must run the same reviewed text.

## Decision

- Harold reads ClickHouse only for the factory flow page, with one fixed named query, `docs/references/telemetry/queries/factory-flow.sql`, which the build embeds. No other query, and no text from a request, goes into the SQL.
- Harold reads as the user `hyperdx_ro`, which may only `SELECT` from `otel.*`, with the settings `readonly=1` and `max_execution_time=5`. The call has a limit of 5 seconds.
- Harold reads only over a loopback address, `http://localhost:8123/` by default. A `[factory_flow] clickhouse_url` whose host is not a loopback address stops Harold at start.
- The password stays in memory for one request only. Harold reads it from `[factory_flow] users_file` at each load, with the rule of `bin/logq`, and drops it after the request. It is never in a setting, a log line, a span, a test fixture or the page. The code has no default path for the file.

## Consequences

- The factory flow page is the only code in Harold that reads ClickHouse. A new reader needs a new ADR.
- A read gives one of five outcomes: `ok`, `unreachable`, `refused`, `timeout` and `not_set_up`. The counter `harold.factory_flow.reads` counts them, and the card **Factory flow reads** shows them.
- `docs/references/telemetry/queries/factory-flow.sql` is the one source of the numbers. A change to it changes the page and `bin/logq --named factory-flow` together.
- The read cannot write. The user and the settings allow only a read, and the URL stays on this machine.
- `reqwest`, which Harold already uses, sends the request. The change adds no dependency.
