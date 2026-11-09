-- Note: Evidence query 2 of the spec agent-jobs. Parameter {since:DateTime}: the merge time of the spec, in UTC.
-- Waits for: nothing
-- Shows: harold.job.ends with how = 'ended' rose after {since}, and no rise has end_line = 'failed'. The query reads to {since} + 30 days.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 14 days before {since} for the previous point.
-- end_told: a job ended and no end line failed. no_end_yet: no job ended. end_not_told: an end line has end_line = 'failed'.
-- Columns: present (1 or 0), reason (end_told, no_end_yet or end_not_told). One row.
SELECT
    toUInt8(ended > 0 AND failed = 0) AS present,
    multiIf(failed > 0, 'end_not_told', ended > 0, 'end_told', 'no_end_yet') AS reason
FROM (
    SELECT
        sumIf(rise, Attributes['how'] = 'ended' AND TimeUnix > {since:DateTime}) AS ended,
        sumIf(rise, Attributes['how'] = 'ended' AND Attributes['end_line'] = 'failed'
                    AND TimeUnix > {since:DateTime}) AS failed
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
