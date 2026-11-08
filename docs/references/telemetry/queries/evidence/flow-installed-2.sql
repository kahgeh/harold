-- Note: Evidence query 2 of the spec flow-installed. Parameter {since:DateTime}: the merge time of the spec, in UTC.
-- Waits for: nothing
-- Shows: pod.inventory.items with list = released_not_installed has points after {since}, so the release tool exports the specs released and not installed.
-- Columns: present (1 or 0), reason (list_seen or no_list). One row.
SELECT
    toUInt8(count() > 0) AS present,
    if(count() > 0, 'list_seen', 'no_list') AS reason
FROM otel.otel_metrics_gauge
WHERE MetricName = 'pod.inventory.items'
  AND Attributes['list'] = 'released_not_installed'
  AND TimeUnix > {since:DateTime}
