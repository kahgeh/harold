-- Note: A build worker on an Opus model is the first thing to check, because build workers belong on Sonnet. The role `other` above a fifth of the tokens means turns do not reach a pod, and the tokens of subagents are not counted.
-- Waits for: nothing
-- Tokens and turns: the rise of the counters harold.turn.tokens (all four kinds), harold.turns, harold.turn.tokens.by_spec and pod.merges.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 21 days: 14 days of two weeks and 7 days before them for the previous point.
-- A week with no point for a source gives no number for that week, never a 0. Tokens with the model none (the seeded start points) are left out.
-- The rows "share of tokens, <model>" are percentages of all tokens. The row "tokens, build worker on Opus" counts the tokens of the role build_worker on a model whose name has opus.
-- The row "share of tokens, role other" is the percentage of the tokens with the role other.
-- The rows "tokens per merged spec" and "turns per merged spec" divide by the merged specs. A week with no merged spec gives no number.
-- The rows "tokens per merged spec" use harold.turn.tokens.by_spec and pod.merges. They ignore the product filter, because by_spec has no product.
-- The product filter applies to the other rows.
-- Columns: series, this_week, last_week.
WITH r AS (
    SELECT
        'tokens' AS src,
        Attributes['model'] AS model,
        Attributes['role'] AS role,
        if(countIf(TimeUnix > now() - INTERVAL 7 DAY) = 0, NULL, sumIf(rise, TimeUnix > now() - INTERVAL 7 DAY)) AS this_rise,
        if(countIf(TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) = 0, NULL, sumIf(rise, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY)) AS last_rise
    FROM (
        SELECT
            Attributes,
            TimeUnix,
            if(Value >= before, Value - before, Value) AS rise
        FROM (
            SELECT
                Attributes,
                TimeUnix,
                Value,
                lagInFrame(Value, 1, toFloat64(0)) OVER (
                    PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                    ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before
            FROM otel.otel_metrics_sum
            WHERE MetricName = 'harold.turn.tokens'
              AND TimeUnix > now() - INTERVAL 21 DAY
              AND Attributes['model'] != 'none'
              AND $__filters
        )
    )
    GROUP BY model, role
    UNION ALL
    SELECT
        'turns' AS src,
        '' AS model,
        '' AS role,
        if(countIf(TimeUnix > now() - INTERVAL 7 DAY) = 0, NULL, sumIf(rise, TimeUnix > now() - INTERVAL 7 DAY)) AS this_rise,
        if(countIf(TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) = 0, NULL, sumIf(rise, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY)) AS last_rise
    FROM (
        SELECT
            Attributes,
            TimeUnix,
            if(Value >= before, Value - before, Value) AS rise
        FROM (
            SELECT
                Attributes,
                TimeUnix,
                Value,
                lagInFrame(Value, 1, toFloat64(0)) OVER (
                    PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                    ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before
            FROM otel.otel_metrics_sum
            WHERE MetricName = 'harold.turns'
              AND TimeUnix > now() - INTERVAL 21 DAY
              AND $__filters
        )
    )
    GROUP BY model, role
    UNION ALL
    SELECT
        'merges' AS src,
        '' AS model,
        '' AS role,
        if(countIf(TimeUnix > now() - INTERVAL 7 DAY) = 0, NULL, sumIf(rise, TimeUnix > now() - INTERVAL 7 DAY)) AS this_rise,
        if(countIf(TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) = 0, NULL, sumIf(rise, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY)) AS last_rise
    FROM (
        SELECT
            Attributes,
            TimeUnix,
            if(Value >= before, Value - before, Value) AS rise
        FROM (
            SELECT
                Attributes,
                TimeUnix,
                Value,
                lagInFrame(Value, 1, toFloat64(0)) OVER (
                    PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                    ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before
            FROM otel.otel_metrics_sum
            WHERE MetricName = 'pod.merges'
              AND TimeUnix > now() - INTERVAL 21 DAY
              AND $__filters
        )
    )
    GROUP BY model, role
    UNION ALL
    SELECT
        'merges_all' AS src,
        '' AS model,
        '' AS role,
        if(countIf(TimeUnix > now() - INTERVAL 7 DAY) = 0, NULL, sumIf(rise, TimeUnix > now() - INTERVAL 7 DAY)) AS this_rise,
        if(countIf(TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) = 0, NULL, sumIf(rise, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY)) AS last_rise
    FROM (
        SELECT
            Attributes,
            TimeUnix,
            if(Value >= before, Value - before, Value) AS rise
        FROM (
            SELECT
                Attributes,
                TimeUnix,
                Value,
                lagInFrame(Value, 1, toFloat64(0)) OVER (
                    PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                    ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before
            FROM otel.otel_metrics_sum
            WHERE MetricName = 'pod.merges'
              AND TimeUnix > now() - INTERVAL 21 DAY
        )
    )
    GROUP BY model, role
    UNION ALL
    SELECT
        'by_spec' AS src,
        '' AS model,
        '' AS role,
        if(countIf(TimeUnix > now() - INTERVAL 7 DAY) = 0, NULL, sumIf(rise, TimeUnix > now() - INTERVAL 7 DAY)) AS this_rise,
        if(countIf(TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) = 0, NULL, sumIf(rise, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY)) AS last_rise
    FROM (
        SELECT
            Attributes,
            TimeUnix,
            if(Value >= before, Value - before, Value) AS rise
        FROM (
            SELECT
                Attributes,
                TimeUnix,
                Value,
                lagInFrame(Value, 1, toFloat64(0)) OVER (
                    PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                    ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before
            FROM otel.otel_metrics_sum
            WHERE MetricName = 'harold.turn.tokens.by_spec'
              AND TimeUnix > now() - INTERVAL 21 DAY
        )
    )
    GROUP BY model, role
)
SELECT series, this_week, last_week
FROM (
    SELECT
        'tokens' AS series,
        toFloat64(sum(this_rise)) AS this_week,
        toFloat64(sum(last_rise)) AS last_week
    FROM r
    WHERE src = 'tokens'
    GROUP BY src
    UNION ALL
    SELECT
        concat('share of tokens, ', model) AS series,
        100 * sum(this_rise) / nullIf(sum(sum(this_rise)) OVER (), 0) AS this_week,
        100 * sum(last_rise) / nullIf(sum(sum(last_rise)) OVER (), 0) AS last_week
    FROM r
    WHERE src = 'tokens'
    GROUP BY model
    UNION ALL
    SELECT
        'tokens, build worker on Opus' AS series,
        if(isNull(sum(this_rise)), NULL, toFloat64(ifNull(sumIf(this_rise, role = 'build_worker' AND positionCaseInsensitive(model, 'opus') > 0), 0))) AS this_week,
        if(isNull(sum(last_rise)), NULL, toFloat64(ifNull(sumIf(last_rise, role = 'build_worker' AND positionCaseInsensitive(model, 'opus') > 0), 0))) AS last_week
    FROM r
    WHERE src = 'tokens'
    GROUP BY src
    UNION ALL
    SELECT
        'share of tokens, role other' AS series,
        100 * sumIf(this_rise, role = 'other') / nullIf(sum(this_rise), 0) AS this_week,
        100 * sumIf(last_rise, role = 'other') / nullIf(sum(last_rise), 0) AS last_week
    FROM r
    WHERE src = 'tokens'
    GROUP BY src
    UNION ALL
    SELECT
        'turns' AS series,
        toFloat64(sum(this_rise)) AS this_week,
        toFloat64(sum(last_rise)) AS last_week
    FROM r
    WHERE src = 'turns'
    GROUP BY src
    UNION ALL
    SELECT
        'merged specs' AS series,
        toFloat64(sum(this_rise)) AS this_week,
        toFloat64(sum(last_rise)) AS last_week
    FROM r
    WHERE src = 'merges'
    GROUP BY src
    UNION ALL
    SELECT
        'tokens per merged spec' AS series,
        sumIf(this_rise, src = 'by_spec') / nullIf(sumIf(this_rise, src = 'merges_all'), 0) AS this_week,
        sumIf(last_rise, src = 'by_spec') / nullIf(sumIf(last_rise, src = 'merges_all'), 0) AS last_week
    FROM r
    WHERE src IN ('by_spec', 'merges_all')
    GROUP BY 1
    UNION ALL
    SELECT
        'turns per merged spec' AS series,
        sumIf(this_rise, src = 'turns') / nullIf(sumIf(this_rise, src = 'merges'), 0) AS this_week,
        sumIf(last_rise, src = 'turns') / nullIf(sumIf(last_rise, src = 'merges'), 0) AS last_week
    FROM r
    WHERE src IN ('turns', 'merges')
    GROUP BY 1
)
GROUP BY series, this_week, last_week
HAVING isNotNull(this_week) OR isNotNull(last_week)
ORDER BY series
