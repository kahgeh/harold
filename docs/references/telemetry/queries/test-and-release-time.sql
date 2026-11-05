-- Note: An ok release attempt over 70 minutes needs a look. So do 2 failed release attempts in a week.
-- Waits for: release-attempts
-- Empty: no data yet (waits for release-attempts)
-- Test and release time: the rise of the counters agent.harold.release.seconds and agent.harold.release.attempts.
-- The rows "ok minutes per attempt, <step>" are the rise of the seconds of the result ok at the step, divided by 60 and by the ok attempts of the week.
-- The row "ok minutes per attempt, all steps" is the sum of those rows.
-- The row "failed attempts" is the rise of the attempts with the result failed. The row "ok attempts" is the rise of the attempts with the result ok.
-- An attempt with the result stopped is in neither row.
-- The counters are declared agent.* counters. Harold keeps their totals, so the query reads totals by the method of agent-counter.sql.
-- A series is one result and one step, so the query takes the rise of each series before it sums. The card has no product.
-- A week with no ok attempt gives no minutes for that week. Columns: series, this_week, last_week.
WITH
    (
        SELECT sum(rise) FROM (
            SELECT greatest(toInt64(maxIf(Value, TimeUnix <= now()) - maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY)), 0) AS rise
            FROM otel.otel_metrics_sum
            WHERE MetricName = 'agent.harold.release.attempts'
              AND Attributes['result'] = 'ok'
              AND TimeUnix > now() - INTERVAL 28 DAY
            GROUP BY Attributes
        )
    ) AS ok_this,
    (
        SELECT sum(rise) FROM (
            SELECT greatest(toInt64(maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY) - maxIf(Value, TimeUnix <= now() - INTERVAL 14 DAY)), 0) AS rise
            FROM otel.otel_metrics_sum
            WHERE MetricName = 'agent.harold.release.attempts'
              AND Attributes['result'] = 'ok'
              AND TimeUnix > now() - INTERVAL 28 DAY
            GROUP BY Attributes
        )
    ) AS ok_last
SELECT
    series,
    sum(this_value) AS this_week,
    sum(last_value) AS last_week
FROM (
    SELECT
        concat('ok minutes per attempt, ', label) AS series,
        this_rise / 60 / nullIf(ok_this, 0) AS this_value,
        last_rise / 60 / nullIf(ok_last, 0) AS last_value
    FROM (
        SELECT
            Attributes['step'] AS step,
            greatest(maxIf(Value, TimeUnix <= now()) - maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY), 0) AS this_rise,
            greatest(maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY) - maxIf(Value, TimeUnix <= now() - INTERVAL 14 DAY), 0) AS last_rise
        FROM otel.otel_metrics_sum
        WHERE MetricName = 'agent.harold.release.seconds'
          AND Attributes['result'] = 'ok'
          AND TimeUnix > now() - INTERVAL 28 DAY
        GROUP BY Attributes
    )
    ARRAY JOIN [step, 'all steps'] AS label
    UNION ALL
    SELECT
        if(Attributes['result'] = 'ok', 'ok attempts', 'failed attempts') AS series,
        toFloat64(greatest(toInt64(maxIf(Value, TimeUnix <= now()) - maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY)), 0)) AS this_value,
        toFloat64(greatest(toInt64(maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY) - maxIf(Value, TimeUnix <= now() - INTERVAL 14 DAY)), 0)) AS last_value
    FROM otel.otel_metrics_sum
    WHERE MetricName = 'agent.harold.release.attempts'
      AND Attributes['result'] IN ('ok', 'failed')
      AND TimeUnix > now() - INTERVAL 28 DAY
    GROUP BY Attributes
)
GROUP BY series
HAVING this_week > 0 OR last_week > 0
ORDER BY series
