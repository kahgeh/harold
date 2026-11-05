-- Note: A build worker on an Opus model is the first thing to check, because build workers belong on Sonnet. The role `other` above a fifth of the tokens means turns do not reach their pod. The tokens of subagents are not counted.
-- Waits for: turn-tokens
-- Empty: no data yet (waits for turn-tokens)
-- Tokens and turns: the rise of the counters harold.turn.tokens (all four kinds), harold.turns, harold.turn.tokens.by_spec and pod.merges.
-- A series is one session, one model or one spec, so the query takes the rise of each series before it sums.
-- The rows "share of tokens, <model>" are percentages of all tokens. The row "tokens, build worker on Opus" counts the tokens of the role build_worker on a model whose name has opus.
-- The row "share of tokens, role other" is the percentage of the tokens with the role other.
-- The rows "tokens per merged spec" and "turns per merged spec" divide by the merged specs. A week with no merged spec gives no number.
-- The rows "tokens per merged spec" use harold.turn.tokens.by_spec and pod.merges. They ignore the product filter, because by_spec has no product.
-- The product filter applies to the other rows. Columns: series, this_week, last_week.
WITH r AS (
    SELECT
        'tokens' AS src,
        Attributes['model'] AS model,
        Attributes['role'] AS role,
        greatest(toInt64(maxIf(Value, TimeUnix <= now()) - maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY)), 0) AS this_rise,
        greatest(toInt64(maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY) - maxIf(Value, TimeUnix <= now() - INTERVAL 14 DAY)), 0) AS last_rise
    FROM otel.otel_metrics_sum
    WHERE MetricName = 'harold.turn.tokens'
      AND TimeUnix > now() - INTERVAL 28 DAY
      AND $__filters
    GROUP BY Attributes
    UNION ALL
    SELECT
        'turns' AS src,
        '' AS model,
        '' AS role,
        greatest(toInt64(maxIf(Value, TimeUnix <= now()) - maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY)), 0) AS this_rise,
        greatest(toInt64(maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY) - maxIf(Value, TimeUnix <= now() - INTERVAL 14 DAY)), 0) AS last_rise
    FROM otel.otel_metrics_sum
    WHERE MetricName = 'harold.turns'
      AND TimeUnix > now() - INTERVAL 28 DAY
      AND $__filters
    GROUP BY Attributes
    UNION ALL
    SELECT
        'merges' AS src,
        '' AS model,
        '' AS role,
        greatest(toInt64(maxIf(Value, TimeUnix <= now()) - maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY)), 0) AS this_rise,
        greatest(toInt64(maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY) - maxIf(Value, TimeUnix <= now() - INTERVAL 14 DAY)), 0) AS last_rise
    FROM otel.otel_metrics_sum
    WHERE MetricName = 'pod.merges'
      AND TimeUnix > now() - INTERVAL 28 DAY
      AND $__filters
    GROUP BY Attributes
    UNION ALL
    SELECT
        'merges_all' AS src,
        '' AS model,
        '' AS role,
        greatest(toInt64(maxIf(Value, TimeUnix <= now()) - maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY)), 0) AS this_rise,
        greatest(toInt64(maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY) - maxIf(Value, TimeUnix <= now() - INTERVAL 14 DAY)), 0) AS last_rise
    FROM otel.otel_metrics_sum
    WHERE MetricName = 'pod.merges'
      AND TimeUnix > now() - INTERVAL 28 DAY
    GROUP BY Attributes
    UNION ALL
    SELECT
        'by_spec' AS src,
        '' AS model,
        '' AS role,
        greatest(toInt64(maxIf(Value, TimeUnix <= now()) - maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY)), 0) AS this_rise,
        greatest(toInt64(maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY) - maxIf(Value, TimeUnix <= now() - INTERVAL 14 DAY)), 0) AS last_rise
    FROM otel.otel_metrics_sum
    WHERE MetricName = 'harold.turn.tokens.by_spec'
      AND TimeUnix > now() - INTERVAL 28 DAY
    GROUP BY Attributes
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
        toFloat64(sumIf(this_rise, role = 'build_worker' AND positionCaseInsensitive(model, 'opus') > 0)) AS this_week,
        toFloat64(sumIf(last_rise, role = 'build_worker' AND positionCaseInsensitive(model, 'opus') > 0)) AS last_week
    FROM r
    WHERE src = 'tokens'
    GROUP BY src
    HAVING sum(this_rise) > 0 OR sum(last_rise) > 0
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
WHERE isNotNull(this_week) OR isNotNull(last_week)
ORDER BY series
