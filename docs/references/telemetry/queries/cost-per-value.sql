-- Note: Cost per value is the money of the 7 days divided by the value of the 7 days. Down is better. A used feature with no kept value counts 1 and shows in unvalued.
-- Waits for: the release that brings the gauge pod.spec.value (until then no spec has a value, and a used feature counts 1).
-- Cost per value: total_cost (AUD) divided by value. Total cost is the token cost, plus the attention cost, plus the cloud cost. Build hours are not in the cost.
-- Value, two parts added together:
--   Features in use: the specs whose counter <product>.feature.used rose in the 7 days (the rule of value-created.sql), each with its weight, the newest point of
--   pod.spec.value (S = 1, M = 2, L = 4), joined on the product of the metric name and spec.id. A feature with no kept value counts 1 and is counted in unvalued.
--   Other specs merged: the specs with a pod.spec.lead_time point of phase = started_to_completed in the 7 days (the merge, recorded once for each spec) whose
--   newest pod.spec.effort point has spec.kind = other, each with its weight. An other spec with no kept value adds nothing.
--   A feature spec counts only by its use, never by its merge. The fixes of pod.fix.confirmed_at add no value.
-- Token cost: the rise of harold.turn.tokens for every role and model other than none (the rule of inventory-cost.sql and tokens-and-turns.sql), priced with the
-- list price (the ClickHouse dictionary harold_settings, read as a table, row harold.list_price.per_mtok/<model>/<kind>, USD per million tokens), then multiplied
-- by the exchange rate (row harold.exchange_rate.usd_to_aud). A price of 0 counts as no price. Without an exchange rate the token cost, the cloud cost and the fix cost are NULL.
-- unpriced_models is the models with tokens in the 7 days and no list price (one text, names with a comma between them, empty when none). They add nothing.
-- Cloud cost: the only cloud spend that Harold has a metric for is the rise of watch.cost_usd (the hosted model runs of the watches, which are not in
-- harold.turn.tokens), times the exchange rate. Harold has no metric for other cloud spend: the other hosted model runs log their cost but send no metric,
-- and Harold does not see bucket or machine bills.
-- Attention cost: the hours of the chief.attention_answered lines (the sum of agent.attr.seconds, divided by 3600) times harold.attention.hourly_rate (100 today).
-- Fix cost: the token cost of harold.turn.tokens.by_spec for the specs whose newest pod.spec.effort point has spec.kind = fix, in AUD. It is part of total_cost.
-- Fix cost and the Waste card are separate. The Waste card (waste.sql) counts hours. Fix cost is AUD and the two are never added together.
-- cost_per_value is NULL when value is 0. cost_per_value_change_pct compares it with the 7 days before and is NULL when either week has no value or no cost.
-- A cost with no spend in its week is NULL, not 0. A column with no number is NULL. The query has no row when the 7 days have neither a cost nor a value.
-- Read windows: the counters (the sums and the histogram) read 21 days: 14 days of two weeks and 7 days before them for the previous point. The gauges read
-- 15 days. The lines read 14 days.
-- Only the newest list prices and exchange rate are kept, so a change of a rate also prices the week before again.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the previous point of its series.
-- A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- Columns: cost_per_value, cost_per_value_change_pct, value, total_cost, token_cost, attention_cost, cloud_cost, fix_cost, unpriced_models, unvalued.
WITH
    (SELECT anyOrNull(value) FROM otel.harold_settings WHERE name = 'harold.attention.hourly_rate') AS rate,
    (SELECT anyOrNull(value) FROM otel.harold_settings WHERE name = 'harold.exchange_rate.usd_to_aud') AS usd_to_aud,
    prices AS (
        SELECT name, value AS price
        FROM otel.harold_settings
        WHERE name LIKE 'harold.list_price.per_mtok/%'
    ),
    rises AS (
        SELECT
            MetricName,
            Attributes,
            TimeUnix,
            if(Value >= before, Value - before, Value) AS rise
        FROM (
            SELECT
                MetricName,
                Attributes,
                TimeUnix,
                Value,
                lagInFrame(Value, 1, toFloat64(0)) OVER (
                    PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                    ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before
            FROM otel.otel_metrics_sum
            WHERE (MetricName IN ('harold.turn.tokens', 'harold.turn.tokens.by_spec', 'watch.cost_usd')
                   OR MetricName LIKE '%.feature.used')
              AND TimeUnix > now() - INTERVAL 21 DAY
        )
    ),
    token_series AS (
        SELECT
            Attributes['model'] AS model,
            Attributes['kind'] AS kind,
            sumIf(rise, TimeUnix > now() - INTERVAL 7 DAY) AS spent_now,
            sumIf(rise, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS spent_before
        FROM rises
        WHERE MetricName = 'harold.turn.tokens' AND Attributes['model'] != 'none'
        GROUP BY model, kind
    ),
    tokens AS (
        SELECT
            if(usd_to_aud IS NULL OR countIf(spent_now > 0) = 0, NULL, sumIf(spent_now * price / 1000000, price > 0) * usd_to_aud) AS cost_now,
            if(usd_to_aud IS NULL OR countIf(spent_before > 0) = 0, NULL, sumIf(spent_before * price / 1000000, price > 0) * usd_to_aud) AS cost_before,
            arrayStringConcat(arraySort(groupUniqArrayIf(model, model != '' AND price = 0 AND spent_now > 0)), ',') AS unpriced
        FROM (
            SELECT model, spent_now, spent_before, concat('harold.list_price.per_mtok/', model, '/', kind) AS name
            FROM token_series
        ) AS series
        LEFT JOIN prices USING (name)
    ),
    fix_specs AS (
        SELECT Attributes['spec.id'] AS spec
        FROM otel.otel_metrics_gauge
        WHERE MetricName = 'pod.spec.effort'
          AND TimeUnix > now() - INTERVAL 15 DAY
        GROUP BY spec
        HAVING argMax(Attributes['spec.kind'], TimeUnix) = 'fix'
    ),
    fixes AS (
        SELECT
            if(usd_to_aud IS NULL OR countIf(spent_now > 0) = 0, NULL, sumIf(spent_now * price / 1000000, price > 0) * usd_to_aud) AS cost_now,
            if(usd_to_aud IS NULL OR countIf(spent_before > 0) = 0, NULL, sumIf(spent_before * price / 1000000, price > 0) * usd_to_aud) AS cost_before
        FROM (
            SELECT
                sumIf(rise, TimeUnix > now() - INTERVAL 7 DAY) AS spent_now,
                sumIf(rise, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS spent_before,
                concat('harold.list_price.per_mtok/', Attributes['model'], '/', Attributes['kind']) AS name
            FROM rises
            WHERE MetricName = 'harold.turn.tokens.by_spec'
              AND Attributes['spec.id'] IN (SELECT spec FROM fix_specs)
            GROUP BY name
        ) AS series
        LEFT JOIN prices USING (name)
    ),
    clouds AS (
        SELECT
            if(usd_to_aud IS NULL OR countIf(TimeUnix > now() - INTERVAL 7 DAY) = 0, NULL,
               sumIf(rise, TimeUnix > now() - INTERVAL 7 DAY) * usd_to_aud) AS cost_now,
            if(usd_to_aud IS NULL OR countIf(TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) = 0, NULL,
               sumIf(rise, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) * usd_to_aud) AS cost_before
        FROM rises
        WHERE MetricName = 'watch.cost_usd'
    ),
    attention AS (
        SELECT
            countIf(Timestamp > now() - INTERVAL 7 DAY) AS answers_now,
            sumIf(toFloat64OrZero(LogAttributes['agent.attr.seconds']), Timestamp > now() - INTERVAL 7 DAY) / 3600 AS hours_now,
            countIf(Timestamp <= now() - INTERVAL 7 DAY) AS answers_before,
            sumIf(toFloat64OrZero(LogAttributes['agent.attr.seconds']), Timestamp <= now() - INTERVAL 7 DAY) / 3600 AS hours_before
        FROM otel.otel_logs
        WHERE Body = 'agent event'
          AND LogAttributes['agent.name'] = 'chief.attention_answered'
          AND Timestamp > now() - INTERVAL 14 DAY
    ),
    weights AS (
        SELECT
            Attributes['pod.product'] AS product,
            Attributes['spec.id'] AS spec,
            argMax(Value, TimeUnix) AS weight
        FROM otel.otel_metrics_gauge
        WHERE MetricName = 'pod.spec.value'
          AND TimeUnix > now() - INTERVAL 15 DAY
        GROUP BY product, spec
    ),
    spec_kinds AS (
        SELECT Attributes['spec.id'] AS spec, argMax(Attributes['spec.kind'], TimeUnix) AS kind
        FROM otel.otel_metrics_gauge
        WHERE MetricName = 'pod.spec.effort'
          AND TimeUnix > now() - INTERVAL 15 DAY
        GROUP BY spec
    ),
    used AS (
        SELECT
            replaceRegexpOne(MetricName, '\\.feature\\.used$', '') AS product,
            Attributes['spec.id'] AS spec,
            maxIf(rise > 0, TimeUnix > now() - INTERVAL 7 DAY) AS used_now,
            maxIf(rise > 0, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS used_before
        FROM rises
        WHERE MetricName LIKE '%.feature.used'
        GROUP BY product, spec
    ),
    merged AS (
        SELECT
            Attributes['pod.product'] AS product,
            Attributes['spec.id'] AS spec,
            maxIf(rise > 0, TimeUnix > now() - INTERVAL 7 DAY) AS merged_now,
            maxIf(rise > 0, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS merged_before
        FROM (
            SELECT
                Attributes,
                TimeUnix,
                if(Count >= before, toInt64(Count) - toInt64(before), toInt64(Count)) AS rise
            FROM (
                SELECT
                    Attributes,
                    TimeUnix,
                    Count,
                    lagInFrame(Count, 1, toUInt64(0)) OVER (
                        PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                        ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before
                FROM otel.otel_metrics_histogram
                WHERE MetricName = 'pod.spec.lead_time'
                  AND Attributes['phase'] = 'started_to_completed'
                  AND TimeUnix > now() - INTERVAL 21 DAY
            )
        )
        GROUP BY product, spec
    ),
    feature_value AS (
        SELECT
            sumIf(if(weight = 0, 1, weight), used_now) AS value_now,
            sumIf(if(weight = 0, 1, weight), used_before) AS value_before,
            countIf(used_now AND weight = 0) AS unvalued_now,
            countIf(used_now OR used_before) AS seen
        FROM used
        LEFT JOIN weights USING (product, spec)
    ),
    other_value AS (
        SELECT
            sumIf(weight, merged_now) AS value_now,
            sumIf(weight, merged_before) AS value_before,
            countIf(merged_now OR merged_before) AS seen
        FROM merged
        INNER JOIN weights USING (product, spec)
        INNER JOIN spec_kinds USING (spec)
        WHERE kind = 'other'
    ),
    totals AS (
        SELECT
            feature_value.value_now + other_value.value_now AS value_now,
            feature_value.value_before + other_value.value_before AS value_before,
            feature_value.unvalued_now AS unvalued,
            tokens.cost_now AS token_now,
            tokens.cost_before AS token_before,
            tokens.unpriced AS unpriced,
            clouds.cost_now AS cloud_now,
            clouds.cost_before AS cloud_before,
            fixes.cost_now AS fix_now,
            if(answers_now = 0 OR rate IS NULL, NULL, hours_now * rate) AS human_now,
            if(answers_before = 0 OR rate IS NULL, NULL, hours_before * rate) AS human_before,
            if(token_now IS NULL AND cloud_now IS NULL AND human_now IS NULL, NULL,
               ifNull(token_now, 0) + ifNull(cloud_now, 0) + ifNull(human_now, 0)) AS total_now,
            if(token_before IS NULL AND cloud_before IS NULL AND human_before IS NULL, NULL,
               ifNull(token_before, 0) + ifNull(cloud_before, 0) + ifNull(human_before, 0)) AS total_before
        FROM feature_value
        CROSS JOIN other_value
        CROSS JOIN tokens
        CROSS JOIN clouds
        CROSS JOIN fixes
        CROSS JOIN attention
    )
SELECT
    if(value_now = 0 OR total_now IS NULL, NULL, round(total_now / value_now, 2)) AS cost_per_value,
    if(value_now = 0 OR value_before = 0 OR ifNull(total_now, 0) = 0 OR ifNull(total_before, 0) = 0, NULL,
       round((total_now / value_now - total_before / value_before) / (total_before / value_before) * 100)) AS cost_per_value_change_pct,
    toFloat64(value_now) AS value,
    if(total_now IS NULL, NULL, round(total_now, 2)) AS total_cost,
    if(token_now IS NULL, NULL, round(token_now, 2)) AS token_cost,
    if(human_now IS NULL, NULL, round(human_now, 2)) AS attention_cost,
    if(cloud_now IS NULL, NULL, round(cloud_now, 2)) AS cloud_cost,
    if(fix_now IS NULL, NULL, round(fix_now, 2)) AS fix_cost,
    unpriced AS unpriced_models,
    unvalued
FROM totals
WHERE total_now IS NOT NULL OR value_now > 0
LIMIT 1
