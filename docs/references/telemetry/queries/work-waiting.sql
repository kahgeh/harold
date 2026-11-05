-- Note: The oldest spec that awaits approval is over 72 hours old when the time you take to approve is too long.
-- Waits for: nothing
-- Work waiting: the last value of each gauge in the week, summed over the products.
-- The rows read pod.inventory.item_days (item-days), pod.inventory.items (specs) and pod.inventory.oldest_hours (the oldest spec, in hours), by list.
-- The row "work records: released" reads harold.work.records at status released, summed over the priorities. It has no product.
-- The product filter applies to the pod.inventory rows only.
-- A gauge with no point in a week gives no number for that week.
-- Columns: series, this_week, last_week.
SELECT
    series,
    if(isNull(max(this_value)), NULL, if(startsWith(series, 'oldest hours'), max(this_value), sum(this_value))) AS this_week,
    if(isNull(max(last_value)), NULL, if(startsWith(series, 'oldest hours'), max(last_value), sum(last_value))) AS last_week
FROM (
    SELECT
        concat(multiIf(MetricName = 'pod.inventory.item_days', 'item-days', MetricName = 'pod.inventory.items', 'specs', 'oldest hours'), ': ', Attributes['list']) AS series,
        argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 7 DAY AND TimeUnix <= now()) AS this_value,
        argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS last_value
    FROM otel.otel_metrics_gauge
    WHERE MetricName IN ('pod.inventory.item_days', 'pod.inventory.items', 'pod.inventory.oldest_hours')
      AND TimeUnix > now() - INTERVAL 14 DAY
      AND $__filters
    GROUP BY MetricName, Attributes
    UNION ALL
    SELECT
        'work records: released' AS series,
        argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 7 DAY AND TimeUnix <= now()) AS this_value,
        argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS last_value
    FROM otel.otel_metrics_gauge
    WHERE MetricName = 'harold.work.records'
      AND Attributes['status'] = 'released'
      AND TimeUnix > now() - INTERVAL 14 DAY
    GROUP BY Attributes
)
GROUP BY series
HAVING isNotNull(this_week) OR isNotNull(last_week)
ORDER BY series
