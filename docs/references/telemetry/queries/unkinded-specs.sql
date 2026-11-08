-- Note: Above 0, the PnEM adds the Kind line of each spec listed. The chief's weekly digest runs this query. No alert.
-- Waits for: nothing
-- Unkinded specs: the spec.id values with spec.kind = unkinded and state approved or later on pod.spec.effort, first seen in the last 14 days.
-- The states after approved are in_progress, completed and withdrawn. The query reads the points of 60 days.
-- A spec that has a point with a kind other than unkinded is not listed: its Kind line was added.
-- Columns: spec_id. One row for each spec.id.
SELECT
    Attributes['spec.id'] AS spec_id
FROM otel.otel_metrics_gauge
WHERE MetricName = 'pod.spec.effort'
  AND TimeUnix > now() - INTERVAL 60 DAY
GROUP BY spec_id
HAVING countIf(Attributes['spec.kind'] != 'unkinded') = 0
   AND minIf(TimeUnix, Attributes['state'] IN ('approved', 'in_progress', 'completed', 'withdrawn')) > now() - INTERVAL 14 DAY
ORDER BY spec_id
