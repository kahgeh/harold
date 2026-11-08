-- Note: None. The value is the number of products with at least one merge in the 7 days.
-- Waits for: nothing
-- Products changed: the products whose merged specs rose in the counter pod.merges. A series is one spec.id, so a new spec.id rises by its whole value.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 21 days: 14 days of two weeks and 7 days before them for the previous point.
-- No row while no product has a merge in the 7 days: the tile shows HyperDX's empty state, not 0.
-- products_change_pct is the percent change against the 7 days before. It is empty when no product had a merge in the 7 days before.
-- Columns: products, products_change_pct.
SELECT
    this_products AS products,
    round((this_products - last_products) / nullIf(last_products, 0) * 100) AS products_change_pct
FROM (
    SELECT
        uniqExactIf(Attributes['pod.product'], rise > 0 AND TimeUnix > now() - INTERVAL 7 DAY) AS this_products,
        uniqExactIf(Attributes['pod.product'], rise > 0 AND TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS last_products
    FROM (
        SELECT
            MetricName,
            Attributes,
            TimeUnix,
            if(Value >= before, Value - before, Value) AS rise
        FROM (
            SELECT
                MetricName,
                Attributes,
                TimeUnix,
                Value,
                lagInFrame(Value, 1, toFloat64(0)) OVER (
                    PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                    ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before
            FROM otel.otel_metrics_sum
            WHERE MetricName = 'pod.merges'
              AND TimeUnix > now() - INTERVAL 21 DAY
        )
    )
)
WHERE this_products > 0
LIMIT 1
