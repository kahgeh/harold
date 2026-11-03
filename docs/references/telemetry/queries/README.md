# Named queries

A **named query** is a SQL file in this folder. A dashboard card and `bin/logq --named <card>` both run it, so a card and its query come from one source. Each query returns this week against last week.

`bin/logq` is the chief's read-only SQL tool. Its `--named` option is a change in the chief's own repository. Harold holds no ClickHouse login.

## The queries

| File | What it shows | Parameter |
| --- | --- | --- |
| `agent-counter.sql` | A declared counter of the [telemetry tool](../../tools/telemetry.md), this week against last week | `{name:String}`: the exported metric name, such as `agent.harold.release.attempts` |

## The columns

Every query returns one row for each series, with these columns:

| Column | Meaning |
| --- | --- |
| `series` | The series' attributes as `key=value` pairs, sorted by key and joined by commas. Empty for a counter with no attributes |
| `this_week` | The count in the last 7 days |
| `last_week` | The count in the 7 days before |

A new query keeps these three column names.

## How `agent-counter.sql` counts

An exported counter is cumulative: each point holds the total since the process started. The query reads the total at three times: now, 7 days ago and 14 days ago. It takes the highest `Value` at or before each time, within the last 28 days. A week's count is the difference between two totals.

- The query does not use `StartTimeUnix`. After the telemetry tool forgets a signal, other counters restart their start time and keep their total.
- A Harold restart resets a cumulative total to 0. A week with a restart can count too low.
- A series with no point yet counts as 0. The query never returns a negative number.

## Run it

The name is a ClickHouse query parameter, `{name:String}`. Pass the metric name with the `agent.` prefix, such as `agent.harold.release.attempts`. How `bin/logq --named` takes a parameter is the chief's choice. Nobody tested `--named` yet: it waits for real use.
