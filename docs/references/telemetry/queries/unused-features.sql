-- Note: A feature with 0 uses for 14 days after its release is a candidate to remove or fix. The chief's weekly digest runs this query. No alert.
-- Waits for: nothing
-- Unused features: the spec.id values of the counters <product>.feature.used whose rise in the last 14 days is 0.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 28 days: 14 days of the window and 14 days before them for the previous point.
-- Columns: spec_id. One row for each spec.id.
SELECT
    spec AS spec_id
FROM (
    SELECT
        Attributes['spec.id'] AS spec,
        TimeUnix,
        if(Value >= before, Value - before, Value) AS rise
    FROM (
        SELECT
            Attributes,
            TimeUnix,
            Value,
            lagInFrame(Value, 1, toFloat64(0)) OVER (
                PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before
        FROM otel.otel_metrics_sum
        WHERE MetricName LIKE '%.feature.used'
          AND TimeUnix > now() - INTERVAL 28 DAY
    )
)
GROUP BY spec
HAVING countIf(rise > 0 AND TimeUnix > now() - INTERVAL 14 DAY) = 0
ORDER BY spec
