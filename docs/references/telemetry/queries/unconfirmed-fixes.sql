-- Note: A fix released over 7 days ago with no confirmed file needs a look. The chief's weekly digest runs this query. No alert.
-- Waits for: evidence-follow-up (until it merges, no confirmed file exists, so every released fix shows here)
-- Unconfirmed fixes: the spec.id values with spec.kind = fix on pod.spec.effort and a merged_to_released point of pod.spec.lead_time
-- whose first point is older than 7 days, and no pod.fix.confirmed_at series.
-- The first point of the merged_to_released series of a spec.id is the time of its release. The query reads the points of 60 days.
-- Columns: spec_id. One row for each spec.id.
SELECT
    Attributes['spec.id'] AS spec_id
FROM otel.otel_metrics_histogram
WHERE MetricName = 'pod.spec.lead_time'
  AND Attributes['phase'] = 'merged_to_released'
  AND TimeUnix > now() - INTERVAL 60 DAY
  AND Attributes['spec.id'] IN (
      SELECT Attributes['spec.id']
      FROM otel.otel_metrics_gauge
      WHERE MetricName = 'pod.spec.effort'
        AND Attributes['spec.kind'] = 'fix'
        AND TimeUnix > now() - INTERVAL 60 DAY
  )
  AND Attributes['spec.id'] NOT IN (
      SELECT Attributes['spec.id']
      FROM otel.otel_metrics_gauge
      WHERE MetricName = 'pod.fix.confirmed_at'
        AND TimeUnix > now() - INTERVAL 60 DAY
  )
GROUP BY spec_id
HAVING min(TimeUnix) < now() - INTERVAL 7 DAY
ORDER BY spec_id
