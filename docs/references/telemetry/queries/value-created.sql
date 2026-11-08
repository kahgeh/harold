-- Note: The value is the count of work records that you checked in the 7 days. Work that reached `released` and that you did not use is not in it.
-- Waits for: work-status-history
-- Value created: the work records that you checked: the rise of the counter harold.work.checked, over all priorities.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 21 days: 14 days of two weeks and 7 days before them for the previous point.
-- No row while no work record is checked in the 7 days: the tile shows HyperDX's empty state, not 0.
-- value_change_pct is the percent change against the 7 days before. It is empty when the 7 days before have no work record checked.
-- Columns: value, value_change_pct.
SELECT
    round(this_value) AS value,
    round((this_value - last_value) / nullIf(last_value, 0) * 100) AS value_change_pct
FROM (
    SELECT
        sumIf(rise, TimeUnix > now() - INTERVAL 7 DAY) AS this_value,
        sumIf(rise, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS last_value
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
            WHERE MetricName = 'harold.work.checked'
              AND TimeUnix > now() - INTERVAL 21 DAY
        )
    )
)
WHERE this_value > 0
LIMIT 1
