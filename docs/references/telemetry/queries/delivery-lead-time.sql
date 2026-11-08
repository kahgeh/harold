-- Note: The slowest work record is the first one to check when the lead time grows.
-- Waits for: work-status-history
-- Empty: work rows: no data yet (waits for work-status-history)
-- Delivery lead time in days, as the p50 of the histogram, by the method of the README panel "Waiting for you, p50":
-- each point gives the mean of the lead times recorded since the previous point (rise of Sum divided by rise of Count).
-- The p50 is the median of these means, weighted by the rise of the Count. Merges are minutes apart and Harold exports every 30 seconds,
-- so a rise holds one lead time. Two lead times within one export would stand as their mean.
-- The rows "work: <phase>" read harold.work.lead_time (idea to your check, by phase).
-- The rows "spec: <phase>" read pod.spec.lead_time (spec time, open to merged). They show a number until the work rows have data.
-- The "spec:" rows keep four phases. The phases spec_writing, architect_review and approval_wait split open_to_approved, so they are not rows.
-- The product filter applies to the "spec:" rows only. A week with no new observation gives no number for that week.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 21 days: 14 days of two weeks and 7 days before them for the previous point.
-- Histogram rule: the same rule on the Count and the Sum. A reset is judged by the Count. Each point gives the rise of the Count and the rise of the Sum.
-- Columns: series, this_week, last_week.
SELECT
    series,
    if(countIf(TimeUnix > now() - INTERVAL 7 DAY AND rise_count > 0) = 0, NULL, quantileExactWeightedIf(0.5)(rise_sum / rise_count / 86400, toUInt64(rise_count), TimeUnix > now() - INTERVAL 7 DAY AND rise_count > 0)) AS this_week,
    if(countIf(TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY AND rise_count > 0) = 0, NULL, quantileExactWeightedIf(0.5)(rise_sum / rise_count / 86400, toUInt64(rise_count), TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY AND rise_count > 0)) AS last_week
FROM (
    SELECT
        concat('work: ', Attributes['phase']) AS series,
        TimeUnix,
        rise_count,
        rise_sum
    FROM (
        SELECT
            Attributes,
            TimeUnix,
            if(cnt >= before_cnt, toInt64(cnt) - toInt64(before_cnt), toInt64(cnt)) AS rise_count,
            if(cnt >= before_cnt, total - before_total, total) AS rise_sum
        FROM (
            SELECT
                Attributes,
                TimeUnix,
                Count AS cnt,
                Sum AS total,
                lagInFrame(Count, 1, toUInt64(0)) OVER (
                    PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                    ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before_cnt,
                lagInFrame(Sum, 1, toFloat64(0)) OVER (
                    PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                    ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before_total
            FROM otel.otel_metrics_histogram
            WHERE MetricName = 'harold.work.lead_time'
              AND TimeUnix > now() - INTERVAL 21 DAY
        )
    )
    UNION ALL
    SELECT
        concat('spec: ', Attributes['phase']) AS series,
        TimeUnix,
        rise_count,
        rise_sum
    FROM (
        SELECT
            Attributes,
            TimeUnix,
            if(cnt >= before_cnt, toInt64(cnt) - toInt64(before_cnt), toInt64(cnt)) AS rise_count,
            if(cnt >= before_cnt, total - before_total, total) AS rise_sum
        FROM (
            SELECT
                Attributes,
                TimeUnix,
                Count AS cnt,
                Sum AS total,
                lagInFrame(Count, 1, toUInt64(0)) OVER (
                    PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                    ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before_cnt,
                lagInFrame(Sum, 1, toFloat64(0)) OVER (
                    PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                    ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before_total
            FROM otel.otel_metrics_histogram
            WHERE MetricName = 'pod.spec.lead_time'
              AND TimeUnix > now() - INTERVAL 21 DAY
              AND Attributes['phase'] IN ('open_to_approved', 'approved_to_started', 'started_to_completed', 'merged_to_released')
              AND $__filters
        )
    )
)
GROUP BY series
HAVING isNotNull(this_week) OR isNotNull(last_week)
ORDER BY series
