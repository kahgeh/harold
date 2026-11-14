-- Note: Evidence query 1 of the spec cost-per-value. Parameter {since:DateTime}: the merge time of the spec, in UTC.
-- Waits for: nothing
-- Shows: the gauge pod.spec.value has a point after {since}, and the counter harold.turn.tokens has a point after {since} (so the card has a value and a cost, and cost_per_value is not NULL when the 7 days hold a use).
-- Columns: present (1 when both have a point after {since}, else 0), reason (value_seen, no_value_yet or no_values), value_points, token_points. One row.
-- no_values means the gauge pod.spec.value has no point at any time: no spec has a kept value yet.
SELECT
    toUInt8(value_points > 0 AND token_points > 0) AS present,
    multiIf(value_points > 0 AND token_points > 0, 'value_seen', any_value_points > 0 OR value_points > 0, 'no_value_yet', 'no_values') AS reason,
    value_points,
    token_points
FROM (
    SELECT
        countIf(TimeUnix > {since:DateTime}) AS value_points,
        count() AS any_value_points
    FROM otel.otel_metrics_gauge
    WHERE MetricName = 'pod.spec.value'
      AND TimeUnix > now() - INTERVAL 60 DAY
) AS v
CROSS JOIN (
    SELECT count() AS token_points
    FROM otel.otel_metrics_sum
    WHERE MetricName = 'harold.turn.tokens'
      AND TimeUnix > {since:DateTime}
      AND TimeUnix <= {since:DateTime} + INTERVAL 30 DAY
) AS t
LIMIT 1
