-- Note: The oldest spec that awaits approval is over 3 days (72 hours) old when the time you take to approve is too long.
-- Waits for: nothing
-- Work waiting: the newest export of each gauge, summed over the products. A series that Harold stopped to export does not count.
-- This week reads the newest export of the last 7 days. Last week reads the newest export of the 7 days before them.
-- The rows read pod.inventory.item_days (item-days), pod.inventory.items (specs) and pod.inventory.oldest_hours (the oldest spec, shown in days), by list.
-- The row "item-days: all lists" is the total of the item-days. The row "oldest days: all lists" is the oldest age of any list.
-- The row "work records: released" reads harold.work.records at status released, summed over the priorities. It has no product.
-- The product filter applies to the work-waiting rows only. A week with no export gives no number for that week.
-- Columns: series, this_week, last_week.
SELECT
    concat(what, ': ', list) AS series,
    if(countIf(wk = 'this') = 0, NULL, sumIf(v, wk = 'this')) AS this_week,
    if(countIf(wk = 'last') = 0, NULL, sumIf(v, wk = 'last')) AS last_week
FROM (
    SELECT
        'this' AS wk,
        multiIf(MetricName = 'pod.inventory.item_days', 'item-days', MetricName = 'pod.inventory.items', 'specs', 'oldest days') AS what,
        Attributes['list'] AS list,
        if(MetricName = 'pod.inventory.oldest_hours', max(Value) / 24, sum(Value)) AS v
    FROM otel.otel_metrics_gauge
    WHERE (MetricName, TimeUnix) IN (
          SELECT MetricName, max(TimeUnix) FROM otel.otel_metrics_gauge
          WHERE MetricName IN ('pod.inventory.item_days', 'pod.inventory.items', 'pod.inventory.oldest_hours') AND TimeUnix > now() - INTERVAL 7 DAY
          GROUP BY MetricName
      )
      AND $__filters
    GROUP BY MetricName, list
    UNION ALL
    SELECT
        'this' AS wk,
        if(MetricName = 'pod.inventory.item_days', 'item-days', 'oldest days') AS what,
        'all lists' AS list,
        if(MetricName = 'pod.inventory.item_days', sum(Value), max(Value) / 24) AS v
    FROM otel.otel_metrics_gauge
    WHERE (MetricName, TimeUnix) IN (
          SELECT MetricName, max(TimeUnix) FROM otel.otel_metrics_gauge
          WHERE MetricName IN ('pod.inventory.item_days', 'pod.inventory.oldest_hours') AND TimeUnix > now() - INTERVAL 7 DAY
          GROUP BY MetricName
      )
      AND $__filters
    GROUP BY MetricName
    UNION ALL
    SELECT
        'this' AS wk,
        'work records' AS what,
        'released' AS list,
        sum(Value) AS v
    FROM otel.otel_metrics_gauge
    WHERE MetricName = 'harold.work.records'
      AND Attributes['status'] = 'released'
      AND TimeUnix = (
          SELECT max(TimeUnix) FROM otel.otel_metrics_gauge
          WHERE MetricName = 'harold.work.records' AND TimeUnix > now() - INTERVAL 7 DAY
      )
    GROUP BY MetricName
    UNION ALL
    SELECT
        'last' AS wk,
        multiIf(MetricName = 'pod.inventory.item_days', 'item-days', MetricName = 'pod.inventory.items', 'specs', 'oldest days') AS what,
        Attributes['list'] AS list,
        if(MetricName = 'pod.inventory.oldest_hours', max(Value) / 24, sum(Value)) AS v
    FROM otel.otel_metrics_gauge
    WHERE (MetricName, TimeUnix) IN (
          SELECT MetricName, max(TimeUnix) FROM otel.otel_metrics_gauge
          WHERE MetricName IN ('pod.inventory.item_days', 'pod.inventory.items', 'pod.inventory.oldest_hours') AND TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY
          GROUP BY MetricName
      )
      AND $__filters
    GROUP BY MetricName, list
    UNION ALL
    SELECT
        'last' AS wk,
        if(MetricName = 'pod.inventory.item_days', 'item-days', 'oldest days') AS what,
        'all lists' AS list,
        if(MetricName = 'pod.inventory.item_days', sum(Value), max(Value) / 24) AS v
    FROM otel.otel_metrics_gauge
    WHERE (MetricName, TimeUnix) IN (
          SELECT MetricName, max(TimeUnix) FROM otel.otel_metrics_gauge
          WHERE MetricName IN ('pod.inventory.item_days', 'pod.inventory.oldest_hours') AND TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY
          GROUP BY MetricName
      )
      AND $__filters
    GROUP BY MetricName
    UNION ALL
    SELECT
        'last' AS wk,
        'work records' AS what,
        'released' AS list,
        sum(Value) AS v
    FROM otel.otel_metrics_gauge
    WHERE MetricName = 'harold.work.records'
      AND Attributes['status'] = 'released'
      AND TimeUnix = (
          SELECT max(TimeUnix) FROM otel.otel_metrics_gauge
          WHERE MetricName = 'harold.work.records' AND TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY
      )
    GROUP BY MetricName
)
GROUP BY series
HAVING isNotNull(this_week) OR isNotNull(last_week)
ORDER BY series
