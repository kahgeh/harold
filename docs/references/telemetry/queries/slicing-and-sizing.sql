-- Note: Medium specs over 8 hours are the first thing to check. A third or more of the medium specs over 8 hours in a week means the sizes are too small.
-- Waits for: work-status-history
-- Empty: no data yet (waits for work-status-history)
-- Slicing and sizing: the share of the specs of each size that are over the limit of the size, in percent.
-- It reads pod.spec.effort at measure build_worker_hours: the last value of each spec in the week. The limit is 4 hours for small and 8 hours for medium.
-- The sizes large and unsized have no limit, so the card has no share for them. The rows "specs counted" give the number of specs of each size.
-- The row "slices per work record" is the mean of the histogram harold.work.slices: the rise of Sum divided by the rise of Count. It has no product.
-- The product filter applies to the pod.spec.effort rows only. A week with no number for a row gives no number for that week.
-- Columns: series, this_week, last_week.
SELECT
    series,
    sum(this_value) AS this_week,
    sum(last_value) AS last_week
FROM (
    SELECT
        'specs counted: ' || size AS series,
        if(countIf(isNotNull(this_v)) = 0, NULL, toFloat64(countIf(isNotNull(this_v)))) AS this_value,
        if(countIf(isNotNull(last_v)) = 0, NULL, toFloat64(countIf(isNotNull(last_v)))) AS last_value
    FROM (
        SELECT
            Attributes['spec.size'] AS size,
            argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 7 DAY AND TimeUnix <= now()) AS this_v,
            argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS last_v
        FROM otel.otel_metrics_gauge
        WHERE MetricName = 'pod.spec.effort'
          AND Attributes['measure'] = 'build_worker_hours'
          AND Attributes['spec.size'] IN ('small', 'medium')
          AND TimeUnix > now() - INTERVAL 14 DAY
          AND $__filters
        GROUP BY Attributes['spec.size'], Attributes['spec.id']
    )
    GROUP BY size
    UNION ALL
    SELECT
        'over limit, share of specs (%): ' || size AS series,
        100.0 * countIf(this_v > if(size = 'small', 4, 8)) / nullIf(countIf(isNotNull(this_v)), 0) AS this_value,
        100.0 * countIf(last_v > if(size = 'small', 4, 8)) / nullIf(countIf(isNotNull(last_v)), 0) AS last_value
    FROM (
        SELECT
            Attributes['spec.size'] AS size,
            argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 7 DAY AND TimeUnix <= now()) AS this_v,
            argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS last_v
        FROM otel.otel_metrics_gauge
        WHERE MetricName = 'pod.spec.effort'
          AND Attributes['measure'] = 'build_worker_hours'
          AND Attributes['spec.size'] IN ('small', 'medium')
          AND TimeUnix > now() - INTERVAL 14 DAY
          AND $__filters
        GROUP BY Attributes['spec.size'], Attributes['spec.id']
    )
    GROUP BY size
    UNION ALL
    SELECT
        'slices per work record' AS series,
        if(sum(this_count) = 0, NULL, sum(this_sum) / sum(this_count)) AS this_value,
        if(sum(last_count) = 0, NULL, sum(last_sum) / sum(last_count)) AS last_value
    FROM (
        SELECT
            greatest(toInt64(maxIf(Count, TimeUnix <= now()) - maxIf(Count, TimeUnix <= now() - INTERVAL 7 DAY)), 0) AS this_count,
            greatest(maxIf(Sum, TimeUnix <= now()) - maxIf(Sum, TimeUnix <= now() - INTERVAL 7 DAY), 0) AS this_sum,
            greatest(toInt64(maxIf(Count, TimeUnix <= now() - INTERVAL 7 DAY) - maxIf(Count, TimeUnix <= now() - INTERVAL 14 DAY)), 0) AS last_count,
            greatest(maxIf(Sum, TimeUnix <= now() - INTERVAL 7 DAY) - maxIf(Sum, TimeUnix <= now() - INTERVAL 14 DAY), 0) AS last_sum
        FROM otel.otel_metrics_histogram
        WHERE MetricName = 'harold.work.slices'
          AND TimeUnix > now() - INTERVAL 28 DAY
        GROUP BY Attributes
    )
    GROUP BY 1
)
GROUP BY series
HAVING isNotNull(this_week) OR isNotNull(last_week)
ORDER BY series
