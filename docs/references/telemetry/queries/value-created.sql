-- Note: The count is for work records that you checked. Work that reached `released` and that you did not use is not in this count.
-- Waits for: work-status-history
-- Empty: no data yet (waits for work-status-history)
-- Work records checked by you: the rise of the counter harold.work.checked, by priority.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 21 days: 14 days of two weeks and 7 days before them for the previous point.
-- A week with no point gives no number for that week.
-- Columns: series, this_week, last_week.
SELECT
    Attributes['priority'] AS series,
    if(countIf(TimeUnix > now() - INTERVAL 7 DAY) = 0, NULL, sumIf(rise, TimeUnix > now() - INTERVAL 7 DAY)) AS this_week,
    if(countIf(TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) = 0, NULL, sumIf(rise, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY)) AS last_week
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
        WHERE MetricName = 'harold.work.checked'
          AND TimeUnix > now() - INTERVAL 21 DAY
    )
)
GROUP BY series
HAVING this_week > 0 OR last_week > 0
ORDER BY series
