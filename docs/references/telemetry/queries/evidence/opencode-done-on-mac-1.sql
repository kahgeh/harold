-- Note: Evidence for the spec opencode-done-on-mac. It needs one opencode turn on this Mac in the 7 days after the release.
-- Waits for: nothing
-- Done seen on this Mac: present = 1 when harold.feature.used with spec.id = 'opencode-done-on-mac' rose after {since:String} (UTC time).
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 7 days after {since:String}. It starts 1 day earlier to find the previous point of each series.
-- Columns: present (0 or 1), reason (done_seen or no_turn_yet). One row.
SELECT
    if(total > 0, 1, 0) AS present,
    if(total > 0, 'done_seen', 'no_turn_yet') AS reason
FROM (
    SELECT
        sumIf(rise, TimeUnix > parseDateTimeBestEffort({since:String})) AS total
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
              AND Attributes['spec.id'] = 'opencode-done-on-mac'
              AND TimeUnix > parseDateTimeBestEffort({since:String}) - INTERVAL 1 DAY
              AND TimeUnix <= parseDateTimeBestEffort({since:String}) + INTERVAL 7 DAY
        )
    )
)
