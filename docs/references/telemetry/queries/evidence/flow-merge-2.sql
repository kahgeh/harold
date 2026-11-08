-- Note: Evidence query 2 of the spec flow-merge. Parameter {since:DateTime}: the merge time of the spec, in UTC.
-- Waits for: nothing
-- Shows: pod.inventory.items with list = ready_not_merged has points after {since}, so pod stats exports the list. A point of value 0 counts.
-- Columns: present (1 or 0), reason (list_seen or no_list). One row.
SELECT
    toUInt8(count() > 0) AS present,
    if(count() > 0, 'list_seen', 'no_list') AS reason
FROM otel.otel_metrics_gauge
WHERE MetricName = 'pod.inventory.items'
  AND Attributes['list'] = 'ready_not_merged'
  AND TimeUnix > {since:DateTime}
