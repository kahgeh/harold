-- Note: None. The card shows the merged specs of each product.
-- Waits for: nothing
-- Merged specs for each product: the rise of the counter pod.merges, summed over the series of the product.
-- A series is one spec.id, so a new spec.id rises by its whole value.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 21 days: 14 days of two weeks and 7 days before them for the previous point.
-- Columns: series, this_week, last_week.
SELECT
    Attributes['pod.product'] AS series,
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
        WHERE MetricName = 'pod.merges'
          AND TimeUnix > now() - INTERVAL 21 DAY
          AND $__filters
    )
)
GROUP BY series
HAVING this_week > 0 OR last_week > 0
ORDER BY series
