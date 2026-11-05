-- Note: The slowest work record is the first one to check when the lead time grows.
-- Waits for: work-status-history
-- Empty: no data yet (waits for work-status-history)
-- Delivery lead time in days, as the mean of the histogram: the rise of Sum divided by the rise of Count.
-- The rows "work: <phase>" read harold.work.lead_time (idea to your check, by phase).
-- The rows "spec: <phase>" read pod.spec.lead_time (spec time, open to merged). They show a number until the work rows have data.
-- The product filter applies to the "spec:" rows only. A row with no new observation in a week is not returned for that week.
-- Columns: series, this_week, last_week.
SELECT
    series,
    if(sum(this_count) = 0, NULL, sum(this_sum) / sum(this_count) / 86400) AS this_week,
    if(sum(last_count) = 0, NULL, sum(last_sum) / sum(last_count) / 86400) AS last_week
FROM (
    SELECT
        concat('work: ', Attributes['phase']) AS series,
        greatest(toInt64(maxIf(Count, TimeUnix <= now()) - maxIf(Count, TimeUnix <= now() - INTERVAL 7 DAY)), 0) AS this_count,
        greatest(maxIf(Sum, TimeUnix <= now()) - maxIf(Sum, TimeUnix <= now() - INTERVAL 7 DAY), 0) AS this_sum,
        greatest(toInt64(maxIf(Count, TimeUnix <= now() - INTERVAL 7 DAY) - maxIf(Count, TimeUnix <= now() - INTERVAL 14 DAY)), 0) AS last_count,
        greatest(maxIf(Sum, TimeUnix <= now() - INTERVAL 7 DAY) - maxIf(Sum, TimeUnix <= now() - INTERVAL 14 DAY), 0) AS last_sum
    FROM otel.otel_metrics_histogram
    WHERE MetricName = 'harold.work.lead_time'
      AND TimeUnix > now() - INTERVAL 28 DAY
    GROUP BY Attributes
    UNION ALL
    SELECT
        concat('spec: ', Attributes['phase']) AS series,
        greatest(toInt64(maxIf(Count, TimeUnix <= now()) - maxIf(Count, TimeUnix <= now() - INTERVAL 7 DAY)), 0) AS this_count,
        greatest(maxIf(Sum, TimeUnix <= now()) - maxIf(Sum, TimeUnix <= now() - INTERVAL 7 DAY), 0) AS this_sum,
        greatest(toInt64(maxIf(Count, TimeUnix <= now() - INTERVAL 7 DAY) - maxIf(Count, TimeUnix <= now() - INTERVAL 14 DAY)), 0) AS last_count,
        greatest(maxIf(Sum, TimeUnix <= now() - INTERVAL 7 DAY) - maxIf(Sum, TimeUnix <= now() - INTERVAL 14 DAY), 0) AS last_sum
    FROM otel.otel_metrics_histogram
    WHERE MetricName = 'pod.spec.lead_time'
      AND TimeUnix > now() - INTERVAL 28 DAY
      AND $__filters
    GROUP BY Attributes
)
GROUP BY series
HAVING sum(this_count) > 0 OR sum(last_count) > 0
ORDER BY series
