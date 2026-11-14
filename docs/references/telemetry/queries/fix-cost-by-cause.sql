-- Note: One cause with 2 fixes or more in 30 days is the thing to look at. This is a diagnosis query, not a card.
-- Waits for: the release that brings the attribute spec.cause on pod.spec.effort and the gauge pod.spec.value.
-- Fix cost by cause: for the fix specs that sent a pod.spec.effort point in the last 30 days, the cause (the spec.cause attribute: the spec id
-- of the merge that brought the fault, or unknown), the number of fixes and their cost in AUD, newest cause first.
-- Fix cost is the token cost of harold.turn.tokens.by_spec for the fix specs, priced as cost-per-value.sql prices it: the list price (the ClickHouse dictionary
-- harold_settings, read as a table, row harold.list_price.per_mtok/<model>/<kind>; a price of 0 counts as no price) times the exchange rate (row harold.exchange_rate.usd_to_aud).
-- The cost is the rise of the counter in the 30 days. The query reads the points of 37 days of the counter, so the first rise has a previous point.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. A first point, or a point below the previous one
-- (Harold restarted), rises by its whole value. The gauge pod.spec.effort reads the points of 30 days.
-- A price that is missing leaves the cost of that series out. Without an exchange rate fix_cost is NULL.
-- "Newest first" means the cause whose newest fix spec has the newest pod.spec.effort point.
-- Columns: cause, fixes, fix_cost, last_seen.
WITH
    (SELECT anyOrNull(value) FROM otel.harold_settings WHERE name = 'harold.exchange_rate.usd_to_aud') AS usd_to_aud,
    prices AS (
        SELECT name, value AS price
        FROM otel.harold_settings
        WHERE name LIKE 'harold.list_price.per_mtok/%'
    ),
    fix_specs AS (
        SELECT
            Attributes['spec.id'] AS spec,
            argMax(Attributes['spec.cause'], TimeUnix) AS cause,
            max(TimeUnix) AS last_seen
        FROM otel.otel_metrics_gauge
        WHERE MetricName = 'pod.spec.effort'
          AND Attributes['spec.kind'] = 'fix'
          AND TimeUnix > now() - INTERVAL 30 DAY
        GROUP BY spec
    ),
    spent AS (
        SELECT
            spec,
            sumIf(rise * price / 1000000, price > 0) AS usd
        FROM (
            SELECT
                Attributes['spec.id'] AS spec,
                if(Value >= before, Value - before, Value) AS rise,
                concat('harold.list_price.per_mtok/', Attributes['model'], '/', Attributes['kind']) AS name
            FROM (
                SELECT
                    Attributes,
                    TimeUnix,
                    Value,
                    lagInFrame(Value, 1, toFloat64(0)) OVER (
                        PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                        ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before
                FROM otel.otel_metrics_sum
                WHERE MetricName = 'harold.turn.tokens.by_spec'
                  AND TimeUnix > now() - INTERVAL 37 DAY
            )
            WHERE TimeUnix > now() - INTERVAL 30 DAY
        ) AS series
        LEFT JOIN prices USING (name)
        GROUP BY spec
    )
SELECT
    if(cause = '', 'unknown', cause) AS cause,
    count() AS fixes,
    if(usd_to_aud IS NULL, NULL, round(sum(usd) * usd_to_aud, 2)) AS fix_cost,
    max(last_seen) AS last_seen
FROM fix_specs
LEFT JOIN spent USING (spec)
GROUP BY cause
ORDER BY last_seen DESC
LIMIT 100
