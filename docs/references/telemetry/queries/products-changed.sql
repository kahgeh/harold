-- Note: None. The card shows the merged specs of each product.
-- Waits for: nothing
-- Merged specs for each product: the rise of the counter pod.merges, summed over the series of the product.
-- A series is one spec.id, so the query takes the rise of each series before it sums.
-- Columns: series, this_week, last_week.
SELECT
    series,
    sum(this_rise) AS this_week,
    sum(last_rise) AS last_week
FROM (
    SELECT
        Attributes['pod.product'] AS series,
        greatest(toInt64(maxIf(Value, TimeUnix <= now()) - maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY)), 0) AS this_rise,
        greatest(toInt64(maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY) - maxIf(Value, TimeUnix <= now() - INTERVAL 14 DAY)), 0) AS last_rise
    FROM otel.otel_metrics_sum
    WHERE MetricName = 'pod.merges'
      AND TimeUnix > now() - INTERVAL 28 DAY
      AND $__filters
    GROUP BY Attributes
)
GROUP BY series
HAVING this_week > 0 OR last_week > 0
ORDER BY series
