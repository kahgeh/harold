-- Note: Evidence query 1 of the spec evidence-check. Parameter {since:DateTime}: the merge time of the spec, in UTC.
-- Waits for: nothing
-- Shows: pod.evidence.missing has a point for the spec.id evidence-check after {since}. Pod stats exports the point once the evidence
-- check writes this spec's own evidence file, in any state. So the query does not wait for a final state.
-- Columns: present (1 or 0), reason (gauge_seen or no_gauge). One row.
SELECT
    toUInt8(count() > 0) AS present,
    if(count() > 0, 'gauge_seen', 'no_gauge') AS reason
FROM otel.otel_metrics_gauge
WHERE MetricName = 'pod.evidence.missing'
  AND Attributes['spec.id'] = 'evidence-check'
  AND TimeUnix > {since:DateTime}
