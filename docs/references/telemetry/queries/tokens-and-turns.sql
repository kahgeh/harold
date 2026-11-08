-- Note: A build worker on an Opus model is the first thing to check, because build workers belong on Sonnet. The tokens of subagents are not counted.
-- Waits for: nothing
-- Tokens and turns is the flow: all tokens spent in the 7 days, on all work. Inventory cost is the stock.
-- The tokens are the rise of the counter harold.turn.tokens (all four kinds, models other than none: the seeded start points are left out), in M with one decimal.
-- The turns are the rise of the counter harold.turns.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 21 days: 14 days of two weeks and 7 days before them for the previous point.
-- A source with no point in the 7 days gives no number (the column is NULL, and its tile shows HyperDX's empty state). It never gives 0.
-- A change column is empty when the 7 days before have no number.
-- overflow is 1 when harold.turn.tokens.by_spec has its overflow series (otel.metric.overflow = true) with a point in the 7 days, and 0 otherwise.
-- With an overflow series the per spec tokens are a lower bound.
-- Columns: tokens_m, tokens_m_change_pct, turns, turns_change_pct, overflow.
SELECT
    if(tokens_n = 0, NULL, round(tokens_now / 1000000, 1)) AS tokens_m,
    round((tokens_now - tokens_before) / nullIf(tokens_before, 0) * 100) AS tokens_m_change_pct,
    if(turns_n = 0, NULL, round(turns_now)) AS turns,
    round((turns_now - turns_before) / nullIf(turns_before, 0) * 100) AS turns_change_pct,
    toUInt8(overflow_n > 0) AS overflow
FROM (
    SELECT
        sumIf(rise, MetricName = 'harold.turn.tokens' AND Attributes['model'] != 'none' AND TimeUnix > now() - INTERVAL 7 DAY) AS tokens_now,
        sumIf(rise, MetricName = 'harold.turn.tokens' AND Attributes['model'] != 'none' AND TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS tokens_before,
        countIf(MetricName = 'harold.turn.tokens' AND Attributes['model'] != 'none' AND TimeUnix > now() - INTERVAL 7 DAY) AS tokens_n,
        sumIf(rise, MetricName = 'harold.turns' AND TimeUnix > now() - INTERVAL 7 DAY) AS turns_now,
        sumIf(rise, MetricName = 'harold.turns' AND TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS turns_before,
        countIf(MetricName = 'harold.turns' AND TimeUnix > now() - INTERVAL 7 DAY) AS turns_n,
        countIf(MetricName = 'harold.turn.tokens.by_spec' AND Attributes['otel.metric.overflow'] = 'true' AND TimeUnix > now() - INTERVAL 7 DAY) AS overflow_n
    FROM (
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
            WHERE MetricName IN ('harold.turn.tokens', 'harold.turns', 'harold.turn.tokens.by_spec')
              AND TimeUnix > now() - INTERVAL 21 DAY
        )
    )
)
WHERE tokens_n > 0 OR turns_n > 0
LIMIT 1
