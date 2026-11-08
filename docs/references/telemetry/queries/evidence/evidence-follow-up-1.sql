-- Note: Evidence query 1 of the spec evidence-follow-up. Parameter {since:DateTime}: the merge time of the spec, in UTC.
-- Waits for: nothing
-- Shows: Harold typed an evidence line into the chief's pane (harold.evidence.lines with outcome = 'typed' rose after {since}), or the chain runs and no spec needed a line.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its Value minus the previous point of its series.
-- A first point, or a point below the previous one (Harold restarted), rises by its whole value. The query reads the points of 14 days before {since} for the previous point.
-- line_typed: a rise of harold.evidence.lines with outcome = 'typed' after {since}.
-- none_missing: no line was typed, pod.evidence.missing has points after {since} and none of them is 1: the chain runs, and no spec needed a line.
-- line_not_typed: pod.evidence.missing has a point of 1 after {since} and no line was typed. no_gauge: pod.evidence.missing has no point after {since}.
-- Columns: present (1 or 0), reason (line_typed, none_missing, line_not_typed or no_gauge). One row.
WITH
typed AS (
    SELECT countIf(rise > 0 AND TimeUnix > {since:DateTime}) AS rises
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
            WHERE MetricName = 'harold.evidence.lines'
              AND Attributes['outcome'] = 'typed'
              AND TimeUnix > {since:DateTime} - INTERVAL 14 DAY
        )
    )
),
gauge AS (
    SELECT
        count() AS points,
        countIf(Value = 1) AS ones
    FROM otel.otel_metrics_gauge
    WHERE MetricName = 'pod.evidence.missing'
      AND TimeUnix > {since:DateTime}
)
SELECT
    toUInt8((SELECT rises FROM typed) > 0 OR ((SELECT points FROM gauge) > 0 AND (SELECT ones FROM gauge) = 0)) AS present,
    multiIf(
        (SELECT rises FROM typed) > 0, 'line_typed',
        (SELECT points FROM gauge) = 0, 'no_gauge',
        (SELECT ones FROM gauge) = 0, 'none_missing',
        'line_not_typed') AS reason
