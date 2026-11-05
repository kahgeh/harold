# Named queries

A **named query** is a SQL file in this folder. A dashboard card and `bin/logq --named <card>` both run it, so a card and its query come from one source. Each query returns this week against last week. A card has one tile with both columns.

`bin/logq` is the chief's read-only SQL tool. Its `--named` option lives in the chief's own repository. Harold holds no ClickHouse login.

## The queries

| File | What it shows | Parameter |
| --- | --- | --- |
| `agent-counter.sql` | A declared counter of the [telemetry tool](../../tools/telemetry.md), this week against last week | `{name:String}`: the exported metric name, such as `agent.harold.release.attempts` |
| `value-created.sql` | Work records that the user checked: the card "Value created" | none |
| `delivery-lead-time.sql` | The p50 days of each phase, from idea to the user's check, and the spec time from open to merged: the card "Delivery lead time" | none |
| `work-waiting.sql` | The items in each list, the oldest age in days and the item-days, with a total: the card "Work waiting" | none |
| `waste.sql` | Hours of free slots with work waiting, redo and failed release attempts, and the specs revised after approval: the card "Waste" | none |
| `tokens-and-turns.sql` | Tokens, the share of each model, turns, and tokens and turns per merged spec: the card "Tokens and turns" | none |
| `slicing-and-sizing.sql` | The share of specs over the limit of their size, and slices per work record: the card "Slicing and sizing" | none |
| `test-and-release-time.sql` | Minutes of an ok release attempt, by step, and the failed attempts: the card "Test and release time" | none |
| `products-changed.sql` | Merged specs of each product: the card "Products changed" | none |
| `now.sql` | The agents that work, of all agents, by pod: the card "Now" | none |
| `clarification-requests.sql` | The times the user asked the chief what a word or reply meant, by cause: the card "Clarification requests per day, by cause" | none |

The ten files after `agent-counter.sql` are the cards of the dashboard "Harold: the big picture". See [Harold: the big picture](../README.md#harold-the-big-picture). A test keeps each card and its file equal.

`bin/logq --named <card> [k=v ...]` is available in the chief's repository. It reads `<card>.sql` from the folder in `$LOGQ_QUERIES_DIR`, turns `$__filters` into no filter and passes each `k=v` as `{k:String}`. The card names equal the file names. The card applies the product filter. The named query does not. So the two give the same numbers only when the filter is empty.

## The columns

Every query returns one row for each series, with these columns:

| Column | Meaning |
| --- | --- |
| `series` | The series' attributes as `key=value` pairs, sorted by key and joined by commas. Empty for a counter with no attributes |
| `this_week` | The count in the last 7 days |
| `last_week` | The count in the 7 days before |

A new query keeps these three column names.

The ten card files read the counters of Harold (`harold.*` and `pod.*`) with the counter rule of the telemetry README: the sum of the rise of each point, where a drop counts its whole value. They read a gauge at its newest export. Only `agent-counter.sql` and the cards of declared `agent.*` counters read totals, as the next section describes.

## How `agent-counter.sql` counts

An exported counter is cumulative: each point holds the total since the process started. The query reads the total at three times: now, 7 days ago and 14 days ago. It takes the highest `Value` at or before each time, within the last 28 days. A week's count is the difference between two totals.

- The query does not use `StartTimeUnix`. After the telemetry tool forgets a signal, other counters restart their start time and keep their total.
- A Harold restart resets a cumulative total to 0. A week with a restart can count too low.
- A series with no point yet counts as 0. The query never returns a negative number.

## Run it

The name is a ClickHouse query parameter, `{name:String}`. Pass the metric name with the `agent.` prefix, such as `agent.harold.release.attempts`. Run it as `bin/logq --named agent-counter name=agent.harold.release.attempts`.
