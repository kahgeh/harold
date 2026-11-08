-- Note: Evidence query 2 of the spec flow-spec-review. Parameter {since:DateTime}: the merge time of the spec, in UTC.
-- Waits for: nothing
-- Shows: the median of open_to_approved for the specs recorded after {since} is above 60 seconds. Before the draft time rule, most specs showed
-- less than 60 seconds (the median was 57 seconds on 2026-11-08). Pod stats records one open_to_approved for each spec, so a series holds one lead time.
-- A spec counts when the first point of its series is after {since}. The wait of a spec is the Sum of its series divided by the Count.
-- Columns: present (1 or 0), reason (approval_wait_true or approval_wait_zero). One row. No spec after {since} gives approval_wait_zero.
SELECT
    toUInt8(median_s > 60) AS present,
    if(median_s > 60, 'approval_wait_true', 'approval_wait_zero') AS reason
FROM (
    SELECT coalesce(quantileExactIf(0.5)(wait_s, specs.first_at > {since:DateTime}), 0) AS median_s
    FROM (
        SELECT
            min(TimeUnix) AS first_at,
            max(Sum) / max(Count) AS wait_s
        FROM otel.otel_metrics_histogram
        WHERE MetricName = 'pod.spec.lead_time'
          AND Attributes['phase'] = 'open_to_approved'
          AND TimeUnix > {since:DateTime} - INTERVAL 14 DAY
        GROUP BY cityHash64(MetricName, ResourceAttributes, Attributes)
    ) AS specs
)
