-- Note: The count is for work records that you checked. Work that reached `released` and that you did not use is not in this count.
-- Waits for: work-status-history
-- Empty: no data yet (waits for work-status-history)
-- Work records checked by you: the rise of the counter harold.work.checked, by priority.
-- Columns: series, this_week, last_week.
SELECT
    series,
    sum(this_rise) AS this_week,
    sum(last_rise) AS last_week
FROM (
    SELECT
        Attributes['priority'] AS series,
        greatest(toInt64(maxIf(Value, TimeUnix <= now()) - maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY)), 0) AS this_rise,
        greatest(toInt64(maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY) - maxIf(Value, TimeUnix <= now() - INTERVAL 14 DAY)), 0) AS last_rise
    FROM otel.otel_metrics_sum
    WHERE MetricName = 'harold.work.checked'
      AND TimeUnix > now() - INTERVAL 28 DAY
    GROUP BY Attributes
)
GROUP BY series
HAVING this_week > 0 OR last_week > 0
ORDER BY series
