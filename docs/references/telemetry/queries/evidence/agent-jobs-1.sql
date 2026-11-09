-- Note: Evidence query 1 of the spec agent-jobs. Parameter {since:DateTime}: the merge time of the spec, in UTC.
-- Waits for: nothing
-- Shows: harold.feature.used with spec.id = 'agent-jobs' rose after {since}, so an agent recorded a job with `harold tool job add`. The query reads to {since} + 30 days.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 14 days before {since} for the previous point.
-- Columns: present (1 or 0), reason (job_added or no_job_yet). One row.
SELECT
    toUInt8(added > 0) AS present,
    if(added > 0, 'job_added', 'no_job_yet') AS reason
FROM (
    SELECT countIf(rise > 0 AND TimeUnix > {since:DateTime}) AS added
    FROM (
        SELECT
            TimeUnix,
            if(Value >= before, Value - before, Value) AS rise
        FROM (
            SELECT
                TimeUnix,
                Value,
                lagInFrame(Value, 1, toFloat64(0)) OVER (
                    PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                    ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before
            FROM otel.otel_metrics_sum
            WHERE MetricName LIKE '%.feature.used'
              AND Attributes['spec.id'] = 'agent-jobs'
              AND TimeUnix > {since:DateTime} - INTERVAL 14 DAY
              AND TimeUnix <= {since:DateTime} + INTERVAL 30 DAY
        )
    )
)
LIMIT 1
