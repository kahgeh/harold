-- Note: Any day above 3 means the chief's writing needs a look (info only).
-- Waits for: telemetry-record-tool
-- Empty: no data yet (waits for telemetry-record-tool)
-- Clarification requests: the rise of the counter agent.chief.clarification_requests, by cause (unknown_term, no_refresher, unchecked_claim, other).
-- The rows with the cause "all" are the total of all causes for each day. "Any day above 3" reads on the row "busiest day: all".
-- The counter is a declared agent.* counter. Harold keeps its total, so the query reads totals by the method of agent-counter.sql.
-- The rows "per day, mean" are the rise of the week divided by 7. The rows "busiest day" are the largest rise of one day in the week.
-- A day is 24 hours back from now, not a calendar day. The counter has no product, so the product filter does not apply.
-- Until the chief declares the counter, or while it stays at 0, the card has no row.
-- Columns: series, this_week, last_week.
WITH per_cause AS (
    SELECT
        Attributes['cause'] AS cause,
        d,
        greatest(toInt64(maxIf(Value, TimeUnix <= now() - toIntervalDay(d)) - maxIf(Value, TimeUnix <= now() - toIntervalDay(d + 1))), 0) AS day_rise
    FROM otel.otel_metrics_sum
    ARRAY JOIN range(14) AS d
    WHERE MetricName = 'agent.chief.clarification_requests'
      AND TimeUnix > now() - INTERVAL 28 DAY
    GROUP BY Attributes, d
)
SELECT series, this_week, last_week
FROM (
    SELECT
        r.1 AS series,
        toFloat64(r.2) AS this_week,
        toFloat64(r.3) AS last_week
    FROM (
        SELECT
            cause,
            sumIf(day_rise, d < 7) AS this_total,
            sumIf(day_rise, d >= 7) AS last_total,
            maxIf(day_rise, d < 7) AS this_max,
            maxIf(day_rise, d >= 7) AS last_max
        FROM (
            SELECT cause, d, day_rise FROM per_cause
            UNION ALL
            SELECT 'all' AS cause, d, sum(day_rise) AS day_rise FROM per_cause GROUP BY d
        )
        GROUP BY cause
        HAVING this_total > 0 OR last_total > 0
    )
    ARRAY JOIN [
        (concat('per day, mean: ', cause), this_total / 7, last_total / 7),
        (concat('busiest day: ', cause), toFloat64(this_max), toFloat64(last_max))
    ] AS r
)
GROUP BY series, this_week, last_week
ORDER BY series
