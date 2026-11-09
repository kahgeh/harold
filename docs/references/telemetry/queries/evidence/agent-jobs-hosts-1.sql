-- Note: Evidence query 1 of the spec agent-jobs-hosts. Parameter {since:DateTime}: the merge time of the spec, in UTC.
-- Waits for: nothing
-- Shows: harold.job.ends with a non-empty host (a job that ran on a slave) rose after {since}. The query reads to {since} + 30 days.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 14 days before {since} for the previous point.
-- host_end_seen: the master counted the end of a slave's job. no_host_end_yet: it counted none.
-- Columns: present (1 or 0), reason (host_end_seen or no_host_end_yet). One row.
SELECT
    toUInt8(ended > 0) AS present,
    if(ended > 0, 'host_end_seen', 'no_host_end_yet') AS reason
FROM (
    SELECT
        sumIf(rise, Attributes['how'] = 'ended' AND Attributes['host'] != ''
                    AND TimeUnix > {since:DateTime}) AS ended
    FROM (
        SELECT
            TimeUnix,
            Attributes,
            if(Value >= before, Value - before, Value) AS rise
        FROM (
            SELECT
                TimeUnix,
                Attributes,
                Value,
                lagInFrame(Value, 1, toFloat64(0)) OVER (
                    PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                    ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before
            FROM otel.otel_metrics_sum
            WHERE MetricName = 'harold.job.ends'
              AND TimeUnix > {since:DateTime} - INTERVAL 14 DAY
              AND TimeUnix <= {since:DateTime} + INTERVAL 30 DAY
        )
    )
)
LIMIT 1
