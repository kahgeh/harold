-- Note: None. The value is the product features used plus the fixes confirmed in the 7 days.
-- Waits for: evidence-follow-up (until it merges, no confirmed file exists, so no fix counts)
-- Value created: the count of spec.id values whose counter <product>.feature.used rose in the 7 days, plus the count of spec.id values of the gauge
-- pod.fix.confirmed_at whose newest time falls in the 7 days. The change is against the 7 days before.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- After a restart the counter starts again from 0, so only the new uses count. The query reads the points of 21 days: 14 days of two weeks
-- and 7 days before them for the previous point.
-- A fix is a spec.id, not a rise: a restart exports the same confirmed time again, so a fix counts once. The gauge reads the points of 15 days,
-- and takes the largest time of each spec.id.
-- A feature seeded at 0 with no use gives 0. With no feature-use series and no confirmed fix at all there is no row: the tile shows
-- HyperDX's empty state, not 0.
-- value_change_pct is the percent change against the 7 days before. It is empty when the 7 days before have no value.
-- Columns: value, value_change_pct.
WITH
    uses AS (
        SELECT
            uniqExactIf(spec, rise > 0 AND TimeUnix > now() - INTERVAL 7 DAY) AS this_n,
            uniqExactIf(spec, rise > 0 AND TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS last_n,
            countIf(TimeUnix > now() - INTERVAL 14 DAY) AS seen
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
                  AND TimeUnix > now() - INTERVAL 21 DAY
            )
        )
    ),
    fixes AS (
        SELECT
            uniqExactIf(spec, confirmed > toUnixTimestamp(now() - INTERVAL 7 DAY)) AS this_n,
            uniqExactIf(spec, confirmed > toUnixTimestamp(now() - INTERVAL 14 DAY) AND confirmed <= toUnixTimestamp(now() - INTERVAL 7 DAY)) AS last_n,
            count() AS seen
        FROM (
            SELECT
                Attributes['spec.id'] AS spec,
                max(Value) AS confirmed
            FROM otel.otel_metrics_gauge
            WHERE MetricName = 'pod.fix.confirmed_at'
              AND TimeUnix > now() - INTERVAL 15 DAY
            GROUP BY spec
        )
    )
SELECT
    toFloat64(uses.this_n + fixes.this_n) AS value,
    round((uses.this_n + fixes.this_n - (uses.last_n + fixes.last_n)) / nullIf(uses.last_n + fixes.last_n, 0) * 100) AS value_change_pct
FROM uses
CROSS JOIN fixes
WHERE uses.seen + fixes.seen > 0
LIMIT 1
