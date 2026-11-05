-- Note: A day with over 2 slot hours of free slots with work waiting needs a look. So does a spec with over 2 redo hours. A spec with 3 or more revisions after approval was not ready.
-- Waits for: release-attempts
-- Empty: no data yet (waits for release-attempts)
-- Waste: the hours and the counts of the week.
-- The row "free slots, work waiting (slot hours)" reads pod.slots.idle_with_work: the sum of the points times 30, divided by 3600.
-- The row "redo hours" reads pod.spec.effort at measure redo_hours: the last value of each spec in the week, summed.
-- The rows "revised after approval" read pod.spec.effort at measure revised_after_approval: the specs with at least 1 revision, and the specs counted.
-- The row "failed release attempts (hours)" reads agent.harold.release.seconds with a result that is not ok: the rise, divided by 3600. It has no product.
-- The product filter applies to the pod rows only. A source with no point in a week gives no number for that week.
-- Columns: series, this_week, last_week.
SELECT
    series,
    sum(this_value) AS this_week,
    sum(last_value) AS last_week
FROM (
    SELECT
        'free slots, work waiting (slot hours)' AS series,
        if(countIf(TimeUnix > now() - INTERVAL 7 DAY AND TimeUnix <= now()) = 0, NULL, sumIf(Value, TimeUnix > now() - INTERVAL 7 DAY AND TimeUnix <= now()) * 30 / 3600) AS this_value,
        if(countIf(TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) = 0, NULL, sumIf(Value, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) * 30 / 3600) AS last_value
    FROM otel.otel_metrics_gauge
    WHERE MetricName = 'pod.slots.idle_with_work'
      AND TimeUnix > now() - INTERVAL 14 DAY
      AND $__filters
    GROUP BY MetricName
    UNION ALL
    SELECT
        r.1 AS series,
        r.2 AS this_value,
        r.3 AS last_value
    FROM (
        SELECT
            Attributes['measure'] AS measure,
            argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 7 DAY AND TimeUnix <= now()) AS this_v,
            argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS last_v
        FROM otel.otel_metrics_gauge
        WHERE MetricName = 'pod.spec.effort'
          AND Attributes['measure'] IN ('redo_hours', 'revised_after_approval')
          AND TimeUnix > now() - INTERVAL 14 DAY
          AND $__filters
        GROUP BY Attributes['measure'], Attributes['spec.id']
    )
    ARRAY JOIN if(
        measure = 'redo_hours',
        [('redo hours', this_v, last_v)],
        [
            ('revised after approval: specs revised', if(isNull(this_v), NULL, toFloat64(this_v >= 1)), if(isNull(last_v), NULL, toFloat64(last_v >= 1))),
            ('revised after approval: specs counted', if(isNull(this_v), NULL, 1.0), if(isNull(last_v), NULL, 1.0))
        ]
    ) AS r
    UNION ALL
    SELECT
        'failed release attempts (hours)' AS series,
        nullIf(greatest(maxIf(Value, TimeUnix <= now()) - maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY), 0) / 3600, 0) AS this_value,
        nullIf(greatest(maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY) - maxIf(Value, TimeUnix <= now() - INTERVAL 14 DAY), 0) / 3600, 0) AS last_value
    FROM otel.otel_metrics_sum
    WHERE MetricName = 'agent.harold.release.seconds'
      AND Attributes['result'] != 'ok'
      AND TimeUnix > now() - INTERVAL 28 DAY
    GROUP BY Attributes
)
GROUP BY series
HAVING isNotNull(this_week) OR isNotNull(last_week)
ORDER BY series
