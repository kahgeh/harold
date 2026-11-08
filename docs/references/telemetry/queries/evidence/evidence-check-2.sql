-- Note: Evidence query 2 of the spec evidence-check. Parameter {since:DateTime}: the merge time of the spec, in UTC.
-- Waits for: nothing
-- Shows: harold.evidence.reads with outcome = ok rose after {since}, so a read of ClickHouse works on the installed Harold.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 14 days before {since} for the previous point.
-- Columns: present (1 or 0), reason (read_ok or no_ok_read). One row.
SELECT
    toUInt8(countIf(rise > 0 AND TimeUnix > {since:DateTime}) > 0) AS present,
    if(countIf(rise > 0 AND TimeUnix > {since:DateTime}) > 0, 'read_ok', 'no_ok_read') AS reason
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
        WHERE MetricName = 'harold.evidence.reads'
          AND Attributes['outcome'] = 'ok'
          AND TimeUnix > {since:DateTime} - INTERVAL 14 DAY
    )
)
