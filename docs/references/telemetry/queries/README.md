# Named queries

A **named query** is a SQL file in this folder. A dashboard card and `bin/logq --named <card>` both run it, so a card and its query come from one source. A card query returns at most one row of numbers: the value columns and a `_change_pct` column for each change. The tile of the card wraps the file in a `SELECT concat(...)` that makes the line of the card. A query of a table card in "Harold trends" returns this week against last week.

`bin/logq` is the chief's read-only SQL tool. Its `--named` option lives in the chief's own repository. Harold holds no ClickHouse login.

## The queries

| File | What it shows | Parameter |
| --- | --- | --- |
| `agent-counter.sql` | A declared counter of the [telemetry tool](../../tools/telemetry.md), this week against last week | `{name:String}`: the exported metric name, such as `agent.harold.release.attempts` |
| `value-created.sql` | The `spec.id` values whose `*.feature.used` counter rose in the 7 days plus the fixes whose `pod.fix.confirmed_at` time falls in the 7 days, and the change: the card "Value created". Columns `value`, `value_change_pct` | none |
| `unused-features.sql` | The `spec.id` values of `*.feature.used` whose rise in the last 14 days is 0. The chief's weekly digest runs it. Column `spec_id` | none |
| `unconfirmed-fixes.sql` | The fix specs with a `merged_to_released` point older than 7 days and no `pod.fix.confirmed_at` series. The chief's weekly digest runs it. Column `spec_id` | none |
| `unkinded-specs.sql` | The specs with `spec.kind = unkinded` at state `approved` or later, first seen in the last 14 days. The chief's weekly digest runs it. Column `spec_id` | none |
| `delivery-lead-time-tile.sql` | The median days from the first draft of a spec to the release that holds it, for the specs released in the 7 days, and the change: the card "Delivery lead time" of the big picture. Columns `days`, `days_change_pct` | none |
| `delivery-lead-time.sql` | The p50 days of each phase, from idea to the user's check, and the spec time from open to merged: the table "Lead time by phase" on "Harold trends" | none |
| `work-waiting.sql` | The items in each list, the oldest age in days and the item-days, with a total: the card "Work waiting" | none |
| `waste.sql` | Hours of free slots with work waiting plus redo hours, in whole hours, and the change: the card "Waste". Columns `hours`, `hours_change_pct` | none |
| `tokens-and-turns.sql` | Tokens in M, turns, their changes and the overflow flag: the card "Tokens and turns". Columns `tokens_m`, `tokens_m_change_pct`, `turns`, `turns_change_pct`, `overflow` | none |
| `inventory-cost.sql` | The cost in AUD of the work not delivered: the tokens at the list prices converted from USD, plus human attention, with changes and the notes: the card "Inventory cost". Columns `tokens_m`, `tokens_m_change_pct`, `token_cost`, `attention_hours`, `attention_cost`, `attention_cost_change_pct`, `currency`, `attention_requests`, `words_per_request`, `total_cost`, `total_cost_change_pct`, `unpriced_models`, `old_stock_unpriced`, `overflow`. It reads the dictionary `otel.harold_settings`, which must exist and hold the list price rows and the exchange rate row. The dictionary holds only the newest price, so a change of a price or of the exchange rate prices the whole stock again, the stock of 7 days before too. A series from before the model was recorded is priced at the mix of the build workers of the 7 days. Fixture test: `scripts/tests/test_inventory_cost_sql.py` | none |
| `cost-per-value.sql` | The AUD cost of the 7 days (tokens, attention and cloud spend) divided by the value of the 7 days (the weights of the features used and of the other specs merged), and the change: the card "Cost per value". Columns `cost_per_value`, `cost_per_value_change_pct`, `value`, `total_cost`, `token_cost`, `attention_cost`, `cloud_cost`, `fix_cost`, `unpriced_models`, `unvalued` | none |
| `fix-cost-by-cause.sql` | The fix specs of the last 30 days, grouped by the spec that caused them: the fixes and their cost in AUD. A diagnosis query, not a card. Columns `cause`, `fixes`, `fix_cost`, `last_seen` | none |
| `slicing-and-sizing.sql` | The share of specs over the limit of their size, and slices per work record: the card "Slicing and sizing" | none |
| `test-and-release-time.sql` | Minutes of an ok release attempt, by step, and the failed attempts: the card "Test and release time" | none |
| `products-changed.sql` | The number of products with a merge, and the change: the card "Products changed". Columns `products`, `products_change_pct` | none |
| `now.sql` | The agents working, all agents and the specs in progress, in one row, from the last 2 minutes: the dashboard "Harold now". Columns `working`, `all`, `in_progress`, `read_at` | none |
| `clarification-requests.sql` | The times the user asked the chief what a word or reply meant, by cause: the card "Clarification requests per day, by cause" | none |
| `attention-recording.sql` | One row for each of the last 14 days: the attention requests, the answers and the chief's messages sent: the card "Attention requests against chief messages". Columns `day`, `attention_requests`, `answers`, `chief_messages` | none |

Six files are the cards of the dashboard "Harold: the big picture": `value-created.sql`, `delivery-lead-time-tile.sql`, `waste.sql`, `tokens-and-turns.sql`, `products-changed.sql` and `inventory-cost.sql`. See [Harold: the big picture](../README.md#harold-the-big-picture). `now.sql` is the dashboard "Harold now". See [Harold now](../README.md#harold-now). `delivery-lead-time.sql`, `work-waiting.sql`, `slicing-and-sizing.sql`, `test-and-release-time.sql`, `clarification-requests.sql` and `attention-recording.sql` are tables on "Harold trends". A test keeps each tile and its file equal.

Two names look alike. The file `delivery-lead-time.sql` feeds the card "Lead time by phase". The glossary term "delivery lead time" names `delivery-lead-time-tile.sql`.

`bin/logq --named <card> [k=v ...]` is available in the chief's repository. It reads `<card>.sql` from the folder in `$LOGQ_QUERIES_DIR`, turns `$__filters` into no filter and passes each `k=v` as `{k:String}`. The card names equal the file names. No dashboard that reads a named query has a product filter, so a tile and `bin/logq --named <card>` give the same numbers.

## The columns

The files of the six cards of the big picture and `now.sql` return at most one row. Each column is a value of the line of the card, or (a column that ends with `_change_pct`) the percent change against the 7 days before. It is already a percent: 57 means 57%. A column with no number is `NULL`: the tile ends in `WHERE <value> IS NOT NULL`, so it shows HyperDX's empty state, never 0. A query returns no row when its source has no data.

The other files return one row for each series, with these columns:

| Column | Meaning |
| --- | --- |
| `series` | In the six files of "Harold trends", a label for the row, such as `harold` or `tokens per merged spec`. In `agent-counter.sql`, the series' attributes as `key=value` pairs, sorted by key and joined by commas, empty for a counter with no attributes |
| `this_week` | The count in the last 7 days |
| `last_week` | The count in the 7 days before |

A new table query keeps these three column names.

The card files read the counters of Harold (`harold.*` and `pod.*`) with the counter rule of the telemetry README: the sum of the rise of each point, where a drop counts its whole value. They read a gauge at its newest export. Only `agent-counter.sql` and the cards of declared `agent.*` counters read totals, as the next section describes.

## How `agent-counter.sql` counts

An exported counter is cumulative: each point holds the total since the process started. The query reads the total at three times: now, 7 days ago and 14 days ago. It takes the highest `Value` at or before each time, within the last 28 days. A week's count is the difference between two totals.

- The query does not use `StartTimeUnix`. After the telemetry tool forgets a signal, other counters restart their start time and keep their total.
- A Harold restart resets a cumulative total to 0. A week with a restart can count too low.
- A series with no point yet counts as 0. The query never returns a negative number.

## Run it

The name is a ClickHouse query parameter, `{name:String}`. Pass the metric name with the `agent.` prefix, such as `agent.harold.release.attempts`. Run it as `bin/logq --named agent-counter name=agent.harold.release.attempts`.
